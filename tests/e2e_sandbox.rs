//! `taw sandbox` in real panes of a throwaway tmux server, with the fake container runtime as the
//! only runtime the panes can reach (`tests/fixtures/sandbox`): what it gives a profile, what it
//! marks on the pane, what the watcher then sees, and what it refuses. No real runtime runs, no real
//! agent either, and every directory the profile or the agent's settings could reach is a scratch one.
//!
//! A net is under all of it: a folder of refusing stand-ins, one for each container runtime and each
//! agent command taw names, first on every `PATH` a rig sets. That is the panes' own (the server's
//! `default-command` and its global `PATH`, which a hook's command has too), and the one every tmux
//! command and `taw` the rig runs carries, which a pane made with a command by such a client takes
//! as its own (tmux hands a new pane the `PATH` of an unattached client that names its command, and
//! mise puts a real `docker` and the real agents on the runner's). Each stand-in records its call and
//! fails, and a rig fails its test for any call it did not take back itself. A profile of the
//! tests' names the fake runtime by its absolute path.

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::fake_runtime::FakeRuntime;
use common::{Server, ServerOptions, wait_for};
use serde_json::{Value, json};

const SETTLE: Duration = Duration::from_secs(25);

const CONFIG: &str = "[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"off\"\n\n[ui.sound]\nenabled = false\n";

/// A profile of the test's own: what taw gave it, written down (the five variables and nothing
/// else), then the contract and nothing more. `{docker}` is the fake runtime's path (`Rig::profile`).
const BOX: &str = r#"#!/bin/sh
env | grep '^TAW_SANDBOX_' | sort > "$FAKE_PROFILE_ENV"
if [ -n "${TAW_SANDBOX_REPORT:-}" ]; then
  set -- -v "$TAW_SANDBOX_REPORT:/run/taw" -e TAW_REPORT_FILE=/run/taw/session.json box-image "$@"
else
  set -- box-image "$@"
fi
exec '{docker}' run --rm -it "$@"
"#;

/// Claude mid-turn, as `tests/e2e_container.rs` draws it.
const CLAUDE_WORKING: &str = "\u{23fa} Bash(cd /work && ./run-tests.sh)\n  \u{23bf}  Running\u{2026}\n\u{273b} Verifying\u{2026} (12s \u{b7} \u{2193} 1.2k tokens \u{b7} esc to interrupt)\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f}\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt\n";

/// Claude Code at its prompt, as `tests/e2e_container.rs` draws it.
const CLAUDE_IDLE: &str = "\u{23fa} Done. The tests pass and the change is committed.\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f}\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)\n";

/// The container runtimes taw recognises, which the net stands in for by name.
const RUNTIMES: [&str; 5] = ["docker", "podman", "nerdctl", "container", "sbx"];

/// The agents a restore types the command of that have no manifest of their own, beside the ones
/// that do (`taw::detect::bundled_ids`).
const MORE_AGENTS: [&str; 2] = ["omp", "mastracode"];

/// Claude's SessionStart input, as its hook writes it inside a sandbox.
const CLAUDE_START: &str = r#"{"session_id":"4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d","transcript_path":"/home/agent/.claude/projects/-work/4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d.jsonl","cwd":"/work","hook_event_name":"SessionStart","source":"startup"}"#;

/// The script of a net's stand-in, which logs its call to `log`: one line, its name and then its
/// arguments, built first and written in one `printf` (one short append is never split), so that
/// two calls made at once, as two sandboxed starts make, leave two whole lines.
fn net_stub(log: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # A refusing stand-in, first on the PATH of a taw test: a real runtime or agent\n\
         # would have run here.\n\
         line=${{0##*/}}; for w; do line=\"$line $w\"; done; printf '%s\\n' \"$line\" >> '{}'\n\
         echo \"${{0##*/}}: refused by the test's net, which stands in front of every runtime and agent\" >&2\n\
         exit 127\n",
        log.display()
    )
}

/// The refusing stand-ins: one for each runtime and each agent command taw names (an agent's id and
/// the command it starts with), in a folder of their own, and the log of every call they took.
struct Net {
    bin: PathBuf,
    log: PathBuf,
}

impl Net {
    fn install(root: &Path) -> Net {
        let net = Net {
            bin: root.join("net"),
            log: root.join("net.log"),
        };
        std::fs::create_dir_all(&net.bin).unwrap();
        let mut names: Vec<String> = RUNTIMES
            .iter()
            .chain(&MORE_AGENTS)
            .map(|name| name.to_string())
            .collect();
        for id in taw::detect::bundled_ids() {
            names.push(id.to_string());
            names.push(taw::detect::executable(id));
        }
        names.sort();
        names.dedup();
        for name in names {
            let stub = net.bin.join(&name);
            std::fs::write(&stub, net_stub(&net.log)).unwrap();
            crate::common::make_executable(&stub);
        }
        net
    }

    /// Every call a stand-in took, one line each: its name, then its arguments.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The calls so far, which a test made on purpose, and the log emptied of them.
    fn take(&self) -> Vec<String> {
        let calls = self.calls();
        std::fs::write(&self.log, "").unwrap();
        calls
    }
}

impl Drop for Net {
    /// The net's verdict, once the rig's server and what it started have ended (the server is the
    /// rig's first field, and drops before the net): a call a stand-in took that the test did not
    /// take back is a runtime or an agent some command reached by name, and the test fails for it.
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let calls = self.calls();
            assert!(
                calls.is_empty(),
                "a runtime or an agent was run by name, and only the net stood in its way: {calls:?}"
            );
        }
    }
}

/// `word` as a shell reads it back as that one word: in single quotes, a quote in it written `'\''`
/// and a newline `'$'\n''` (the default profile's print mode writes its words the same way), so that
/// the line stays one line and nothing in it is expanded.
fn quoted(word: &str) -> String {
    format!(
        "'{}'",
        word.replace('\'', "'\\''").replace('\n', "'$'\\n''")
    )
}

/// `words` as one line a shell reads back as those very words.
fn line(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| quoted(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A server for `taw sandbox`, its fake runtime, the net in front of everything real, and the file
/// a profile of the tests' writes what it was given to. Fields drop in order: the server first, so
/// that nothing of it runs on in a folder that has gone, then the net, which judges what was run
/// until then (its `Drop`).
struct Rig {
    server: Server,
    net: Net,
    fake: FakeRuntime,
    env_file: PathBuf,
    scratch: PathBuf,
    /// Every pane's `PATH`: the net (after the fake runtime's folder, in a rig that runs the runtime
    /// by name), a folder of the test's after it in a rig given one, then the system's folders, and
    /// nothing of the runner's.
    path: String,
    _tmp: tempfile::TempDir,
}

impl Rig {
    fn new() -> Rig {
        Rig::build(false, &[], None, None)
    }

    /// A rig whose panes find the fake runtime as `docker` by name, ahead of the net: taw's own
    /// profile runs the runtime by name.
    fn with_the_runtime_by_name() -> Rig {
        Rig::build(true, &[], None, None)
    }

    /// A rig whose server, and so every watcher and hook it starts, has `extra` in its environment.
    fn with_env(extra: &[(&str, &str)]) -> Rig {
        Rig::build(false, extra, None, None)
    }

    /// [`Rig::with_the_runtime_by_name`] with `extra` in the server's environment, which every
    /// pane's shell, and so taw's own profile, inherits.
    fn with_the_runtime_by_name_and_env(extra: &[(&str, &str)]) -> Rig {
        Rig::build(true, extra, None, None)
    }

    /// A rig whose watchers find what `dir` holds first on their `PATH`: a `pipe-pane` watcher has the
    /// environment its server was started with, not tmux's global one. `extra` goes in that
    /// environment too.
    fn with_first_on_the_watchers_path(dir: &Path, extra: &[(&str, &str)]) -> Rig {
        Rig::build(false, extra, Some(dir), None)
    }

    /// `Rig::new` with `dir` on every pane's `PATH`, right after the net, and so in the global `PATH`
    /// that `typed::session_path` reads.
    fn with_on_the_path(dir: &Path) -> Rig {
        Rig::build(false, &[], None, Some(dir))
    }

    /// The rig, with the fake runtime's folder ahead of the net when `runtime_by_name`, `extra` in
    /// the server's environment, `first` first on the watchers' `PATH`, and `also` after the net on
    /// every pane's.
    fn build(
        runtime_by_name: bool,
        extra: &[(&str, &str)],
        first: Option<&Path>,
        also: Option<&Path>,
    ) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = std::fs::canonicalize(tmp.path()).unwrap();
        let fake = FakeRuntime::install(&scratch);
        let net = Net::install(&scratch);
        let env_file = scratch.join("profile-env");
        let also = also
            .map(|dir| format!("{}:", dir.display()))
            .unwrap_or_default();
        let path = match runtime_by_name {
            true => format!(
                "{}:{}:{also}/usr/bin:/bin",
                fake.bin.display(),
                net.bin.display()
            ),
            false => format!("{}:{also}/usr/bin:/bin", net.bin.display()),
        };
        // What every tmux command and taw of the rig carries, and the server it starts, and so its
        // watchers: the net in front of the runner's.
        let client_path = format!(
            "{}{}:{}",
            first.map_or(String::new(), |dir| format!("{}:", dir.display())),
            net.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut env = fake.env();
        env.push(("PATH".to_string(), client_path));
        env.push((
            "FAKE_PROFILE_ENV".to_string(),
            env_file.display().to_string(),
        ));
        env.push((
            "FAKE_RUNTIME_STOP".to_string(),
            fake.stop_file().display().to_string(),
        ));
        env.push(("TAW_LOG".to_string(), "debug".to_string()));
        // No tmux is on the panes' PATH: taw in a pane, and the watchers, find it by this.
        let tmux = taw::tmux::Tmux::with_socket_name("unused").bin;
        env.push(("TAW_TMUX_BIN".to_string(), tmux.display().to_string()));
        for (name, dir) in [
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("CODEX_HOME", "codex"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_STATE_HOME", "xdg-state"),
            ("XDG_CACHE_HOME", "xdg-cache"),
        ] {
            let path = scratch.join(dir);
            std::fs::create_dir_all(&path).unwrap();
            env.push((name.to_string(), path.display().to_string()));
        }
        // Last, so that a test's own wins over the rig's (a stand-in for tmux, say).
        env.extend(
            extra
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        );
        let server = Server::start_with(ServerOptions {
            width: 220,
            height: 40,
            env,
            ..ServerOptions::default()
        });
        Rig::give_panes_the_path(&server, &path);
        server.write_config(CONFIG);
        Rig {
            server,
            net,
            fake,
            env_file,
            scratch,
            path,
            _tmp: tmp,
        }
    }

    /// Every pane `server` makes from now on gets `path` as its `PATH`, set from inside its shell
    /// (tmux hands a new pane's process no `PATH` reliably), and tmux keeps it as its global `PATH`,
    /// which a hook's command has.
    fn give_panes_the_path(server: &Server, path: &str) {
        server.tmux_ok(&["set-environment", "-g", "PATH", path]);
        server.tmux_ok(&["set-option", "-g", "default-shell", "/bin/sh"]);
        server.tmux_ok(&[
            "set-option",
            "-g",
            "default-command",
            &format!("export PATH='{path}'; exec /bin/sh"),
        ]);
    }

    /// A profile of yours called `name`, executable, with `{docker}` in `body` the fake's own path.
    fn profile(&self, name: &str, body: &str) -> PathBuf {
        let dir = self.server.config_dir().join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(
            &path,
            body.replace(
                "{docker}",
                &self.fake.bin.join("docker").display().to_string(),
            ),
        )
        .unwrap();
        crate::common::make_executable(&path);
        path
    }

    /// A new window of the server's session, its pane id, once its shell is at its first prompt: a
    /// line typed before the shell is up is read by a shell that is not ready for it, and under load
    /// a `taw sandbox` typed that early was once refused.
    fn window(&self) -> String {
        let pane = self.server.tmux_ok(&[
            "new-window",
            "-t",
            "one",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            self.server.work_dir().to_str().unwrap(),
        ]);
        self.server.wait_for_prompt(&pane);
        pane
    }

    fn type_line(&self, pane: &str, line: &str) {
        self.server.tmux_ok(&["send-keys", "-t", pane, "-l", line]);
        self.server.tmux_ok(&["send-keys", "-t", pane, "Enter"]);
    }

    /// What a person types: the built taw, by its path, and `words`.
    fn sandbox(&self, pane: &str, words: &[&str]) {
        let mut typed = vec![common::taw_bin(), "sandbox"];
        typed.extend_from_slice(words);
        self.type_line(pane, &line(&typed));
    }

    fn wait_for_command(&self, pane: &str, name: &str) -> bool {
        wait_for(
            || self.server.display(pane, "#{pane_current_command}") == name,
            SETTLE,
        )
    }

    /// The report folder taw keeps for `pane` on this server: named for the server by its socket
    /// (`taw::sandbox::report::key_of`), as the server says where it listens.
    fn report(&self, pane: &str) -> PathBuf {
        let socket = self
            .server
            .tmux_ok(&["display-message", "-p", "#{socket_path}"]);
        self.server
            .state_dir()
            .join("sandbox/panes")
            .join(taw::sandbox::report::key_of(&socket))
            .join(pane)
    }

    /// What an agent's integration does inside the sandbox of `pane`: one report, written beside
    /// the file and renamed into place.
    fn report_from_inside(&self, pane: &str, payload: &str) {
        let folder = self.report(pane);
        let staged = folder.join(".session.json.inside");
        std::fs::write(&staged, payload).unwrap();
        std::fs::rename(&staged, folder.join("session.json")).unwrap();
    }

    fn screen_holds(&self, pane: &str, text: &str) -> bool {
        wait_for(|| self.server.capture(pane).contains(text), SETTLE)
    }

    /// Whether the pane's screen holds `text` with its wrapped lines joined: a line longer than the
    /// pane is read whole.
    fn screen_holds_joined(&self, pane: &str, text: &str) -> bool {
        wait_for(
            || {
                self.server
                    .tmux_ok(&["capture-pane", "-p", "-J", "-t", pane])
                    .contains(text)
            },
            SETTLE,
        )
    }

    fn stop(&self) {
        std::fs::write(self.fake.stop_file(), "").unwrap();
    }

    /// The built taw run with `args` from a terminal of its own, outside tmux: no TMUX, no
    /// TMUX_PANE, and nothing else of the runner's, in the rig's scratch folder.
    fn outside(&self, args: &[&str]) -> std::process::Output {
        self.outside_with(args, &[])
    }

    /// [`Rig::outside`], with `extra` in its environment.
    fn outside_with(&self, args: &[&str], extra: &[(&str, &str)]) -> std::process::Output {
        let home = self.scratch.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut cmd = std::process::Command::new(common::taw_bin());
        cmd.args(args)
            .current_dir(&self.scratch)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &home)
            .env("HISTFILE", "/dev/null")
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("TAW_CONFIG_DIR", self.server.config_dir())
            .env("TAW_STATE_DIR", self.server.state_dir())
            .env("FAKE_PROFILE_ENV", &self.env_file);
        for (name, value) in self.fake.env() {
            cmd.env(name, value);
        }
        for (name, value) in extra {
            cmd.env(name, value);
        }
        cmd.output().unwrap()
    }
}

/// What Claude draws, written to the pane's tty: a title and a screen.
fn draw(server: &Server, pane: &str, title: &str, screen: &str) {
    let tty = server.display(pane, "#{pane_tty}");
    let mut out = std::fs::OpenOptions::new().write(true).open(&tty).unwrap();
    write!(
        out,
        "\x1b]2;{title}\x07\x1b[2J\x1b[H{}",
        screen.replace('\n', "\r\n")
    )
    .unwrap();
}

/// How many of the watcher's reads of `pane` had output since the read before and reached a
/// decision: the reads a screen drawn in the pane is seen by, and never one the watcher makes
/// whether or not the pane prints. Counted from the watcher's debug log: `reading pane %3 (412 bytes
/// since the last read, ...)`, then that read's `decision:`.
fn reads_after_output(rig: &Rig, pane: &str) -> usize {
    let log = std::fs::read_to_string(
        rig.server
            .state_dir()
            .join("logs")
            .join(format!("watch-{pane}.log")),
    )
    .unwrap_or_default();
    let start = format!("reading pane {pane} (");
    let (mut after_output, mut count) = (false, 0);
    for line in log.lines() {
        if let Some((_, rest)) = line.split_once(&start) {
            after_output = rest
                .split(' ')
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| n > 0);
        } else if after_output && line.contains(" decision: ") {
            count += 1;
            after_output = false;
        }
    }
    count
}

/// Waits until the watcher of `pane` has made more than `seen` reads after output, and has read all
/// the output the pane made: the count has stayed the same for a second and a half, three times the
/// half second within which a pane with no agent is read once it prints (`watch::Timing`), and long
/// past the writes of the last read, which the watcher makes after it logs the read's decision.
fn read_all_output(rig: &Rig, pane: &str, seen: usize) -> bool {
    wait_for(
        || {
            let before = reads_after_output(rig, pane);
            std::thread::sleep(Duration::from_millis(1500));
            before > seen && reads_after_output(rig, pane) == before
        },
        SETTLE,
    )
}

/// The words a real shell reads from `line`: `shell` runs a script that prints each word of the
/// line, with a scratch home (so that a `~` the line left bare would show) and `place` to run in
/// (where a `$(...)` it ran would leave its file).
fn read_back(shell: &str, line: &str, home: &Path, place: &Path) -> Vec<String> {
    let script = place.join("read-back");
    std::fs::write(&script, format!("printf '%s\\0' {line}\n")).unwrap();
    let mut cmd = std::process::Command::new(shell);
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
        "{shell} could not read {line}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut words: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .map(str::to_string)
        .collect();
    // Each word ends with a NUL, an empty one too: what follows the last is no word.
    words.pop();
    words
}

/// The net's stand-ins take calls at once (two sandboxed starts ask `claude --version` together), and
/// each call is one whole line in the log: 32 calls started together, a hundred times over, leave 32
/// lines each, every one the call as made. A stand-in that wrote its name, its words and its end of
/// line apart would interleave them.
#[test]
fn the_nets_stand_ins_keep_every_line_whole_when_called_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("net.log");
    let stub = tmp.path().join("claude");
    std::fs::write(&stub, net_stub(&log)).unwrap();
    crate::common::make_executable(&stub);
    for _ in 0..100 {
        std::fs::write(&log, "").unwrap();
        let children: Vec<_> = (0..32)
            .map(|_| {
                std::process::Command::new(&stub)
                    .arg("--version")
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            child.wait().unwrap();
        }
        let lines: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines, vec!["claude --version"; 32]);
    }
}

/// The net under the rule. A runtime or an agent run by name reaches a refusing stand-in, which
/// records it, never a real one: typed in a pane of the rig, run by a pane made with a command by
/// the rig's own tmux client with no `PATH` of its own given (it takes that client's), and run by a
/// hook's command. Each is looked up first, and run only once it is seen to be the stand-in.
#[test]
fn the_net_catches_a_runtime_or_an_agent_run_by_name_from_a_pane_or_a_hook() {
    let rig = Rig::new();
    let net = rig.net.bin.display().to_string();
    let in_the_net = |found: &Path, names: &[&str]| {
        assert!(
            wait_for(
                || std::fs::read_to_string(found).is_ok_and(|text| text.ends_with('\n')),
                SETTLE
            ),
            "{} was never written",
            found.display()
        );
        let text = std::fs::read_to_string(found).unwrap();
        let expected: Vec<String> = names.iter().map(|name| format!("{net}/{name}")).collect();
        assert_eq!(text.split_whitespace().collect::<Vec<_>>(), expected);
    };

    // Typed in a pane.
    let pane = rig.window();
    let found = rig.scratch.join("found-typed");
    let part = rig.scratch.join("found-typed.part");
    rig.type_line(
        &pane,
        &format!(
            "command -v docker claude podman codex cursor-agent kiro-cli omp > {} && mv {} {}",
            quoted(&part.display().to_string()),
            quoted(&part.display().to_string()),
            quoted(&found.display().to_string())
        ),
    );
    in_the_net(
        &found,
        &[
            "docker",
            "claude",
            "podman",
            "codex",
            "cursor-agent",
            "kiro-cli",
            "omp",
        ],
    );
    rig.type_line(&pane, "docker info; claude --version");
    assert!(
        wait_for(|| rig.net.calls().len() == 2, SETTLE),
        "{:?}",
        rig.net.calls()
    );

    // Run by a pane made with a command by the rig's own client, which takes that client's PATH.
    let made = rig.scratch.join("found-made");
    rig.server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one",
        &format!(
            "d=$(command -v docker); c=$(command -v claude); printf '%s %s\\n' \"$d\" \"$c\" > {made}.part; mv {made}.part {made}; \
             case \"$d $c\" in {expected}) docker ps; claude -p hi;; esac",
            made = quoted(&made.display().to_string()),
            expected = quoted(&format!("{net}/docker {net}/claude")),
        ),
    ]);
    in_the_net(&made, &["docker", "claude"]);

    // Run by a hook's command, which has the server's global PATH.
    let hooked = rig.scratch.join("found-hook");
    rig.server.tmux_ok(&[
        "run-shell",
        &format!(
            "p=$(command -v podman); echo \"$p\" > {hooked}.part; mv {hooked}.part {hooked}; \
             [ \"$p\" != {expected} ] || podman ps 2>/dev/null; true",
            hooked = quoted(&hooked.display().to_string()),
            expected = quoted(&format!("{net}/podman")),
        ),
    ]);
    in_the_net(&hooked, &["podman"]);

    assert!(
        wait_for(|| rig.net.calls().len() == 5, SETTLE),
        "{:?}",
        rig.net.calls()
    );
    let mut calls = rig.net.take();
    calls.sort();
    assert_eq!(
        calls,
        [
            "claude --version",
            "claude -p hi",
            "docker info",
            "docker ps",
            "podman ps"
        ]
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// A rig fails its test for a call the net took that the test did not take back itself: here an
/// agent's version asked by name in a pane, as the default profile asks it of whatever `claude` its
/// `PATH` finds.
#[test]
#[should_panic(expected = "a runtime or an agent was run by name")]
fn a_test_whose_command_reached_the_net_fails() {
    let rig = Rig::new();
    let pane = rig.window();
    let found = rig.scratch.join("found");
    let part = rig.scratch.join("found.part");
    rig.type_line(
        &pane,
        &format!(
            "command -v claude > {} && mv {} {}",
            quoted(&part.display().to_string()),
            quoted(&part.display().to_string()),
            quoted(&found.display().to_string())
        ),
    );
    assert!(wait_for(|| found.exists(), SETTLE));
    assert_eq!(
        std::fs::read_to_string(&found).unwrap().trim(),
        rig.net.bin.join("claude").display().to_string()
    );

    rig.type_line(&pane, "claude --version");

    assert!(
        wait_for(|| rig.net.calls() == ["claude --version"], SETTLE),
        "{:?}",
        rig.net.calls()
    );
}

/// The net judges what was run until the server and what it started have ended: a job of the
/// server's, as a hook's command is, that runs a runtime by name as the server ends still fails its
/// test.
#[test]
#[should_panic(expected = "a runtime or an agent was run by name")]
fn a_runtime_run_by_name_as_the_server_ends_fails_the_test() {
    let rig = Rig::new();
    let ready = rig.scratch.join("job-ready");
    rig.server.tmux_ok(&[
        "run-shell",
        "-b",
        &format!(
            "trap 'docker ps; exit 0' TERM; : > {}; sleep 60",
            quoted(&ready.display().to_string())
        ),
    ]);
    assert!(wait_for(|| ready.exists(), SETTLE));
}

/// The words the tests type are read back by every shell a pane can have as those very words: a word
/// starting with ~ or =, an assignment with ~ in it, a four-byte character, quotes, an empty word, a
/// space, a newline, a $(...) and what a shell globs or comments.
#[test]
fn a_typed_line_is_read_back_as_its_words_by_sh_bash_3_2_bash_5_and_zsh() {
    let words = [
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
        "line\nbreak",
        "\\",
        "*",
        "#x",
        "!x",
        "--model",
    ];
    let typed = line(&words);
    assert!(!typed.contains('\n'), "{typed}");
    let mut readers = vec!["/bin/sh".to_string(), "/bin/bash".to_string()];
    if let Ok(found) = which::which("bash") {
        let same = std::fs::canonicalize(&found).ok() == std::fs::canonicalize("/bin/bash").ok();
        if !same {
            readers.push(found.display().to_string());
        }
    }
    readers.push(
        which::which("zsh")
            .map(|zsh| zsh.display().to_string())
            .unwrap_or_else(|_| "/bin/zsh".to_string()),
    );
    let dir = tempfile::tempdir().unwrap();
    let (home, place) = (dir.path().join("home"), dir.path().join("place"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&place).unwrap();

    for reader in &readers {
        assert_eq!(
            read_back(reader, &typed, &home, &place),
            words,
            "read by {reader}: {typed}"
        );
    }
    assert!(
        !place.join("pwned").exists(),
        "a shell ran the $(...) it was typed"
    );
}

/// The rig hands a test a new pane only once its shell is at its first prompt, though that shell
/// comes up late (as it does under load): a line a test types before then (`taw sandbox` among them)
/// would be read by a shell that is not ready for it, and the test below would fail.
#[test]
fn a_new_pane_is_handed_over_at_its_shells_first_prompt() {
    let rig = Rig::new();
    rig.server.tmux_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!("export PATH='{}'; sleep 1; exec /bin/sh", rig.path),
    ]);

    let pane = rig.window();

    let screen = rig.server.capture(&pane);
    assert!(common::at_a_prompt(&screen), "{screen:?}");
}

#[test]
fn a_profile_is_given_the_five_variables_and_the_agents_command_line_and_its_pane_is_marked() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    let odd = "it's $(touch pwned) here";

    rig.sandbox(
        &pane,
        &["claude", "--profile", "box", "--", "--model", "opus", odd],
    );

    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    let work = std::fs::canonicalize(rig.server.work_dir()).unwrap();
    let report = rig.report(&pane);
    assert_eq!(
        std::fs::read_to_string(&rig.env_file).unwrap(),
        format!(
            "TAW_SANDBOX_AGENT=claude\nTAW_SANDBOX_DIR={}\nTAW_SANDBOX_PANE={pane}\nTAW_SANDBOX_PROFILE=box\nTAW_SANDBOX_REPORT={}\n",
            work.display(),
            report.display()
        )
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    // The mark's process is taw's own, which became the profile and then the runtime: the pid of
    // what is in front of the pane now, docker.
    let pid = rig.server.pane_opt(&pane, "@agent_sandbox_pid");
    let comm = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&comm.stdout)
            .trim()
            .ends_with("docker"),
        "pid {pid:?}"
    );
    // The fake writes down its run as it starts, which can be a moment after tmux names it.
    assert!(
        wait_for(|| !rig.fake.runs().is_empty(), SETTLE),
        "{:?}",
        rig.fake.calls()
    );
    assert_eq!(
        rig.fake.runs(),
        [vec![
            "docker".to_string(),
            "run".to_string(),
            "--rm".to_string(),
            "-it".to_string(),
            "-v".to_string(),
            format!("{}:/run/taw", report.display()),
            "-e".to_string(),
            "TAW_REPORT_FILE=/run/taw/session.json".to_string(),
            "box-image".to_string(),
            "claude".to_string(),
            "--model".to_string(),
            "opus".to_string(),
            odd.to_string(),
        ]]
    );
    assert!(
        !work.join("pwned").exists(),
        "a shell ran the agent's argument"
    );
    rig.stop();
}

#[test]
fn the_watcher_sees_the_agent_in_its_container_and_agent_list_says_where_it_runs() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(wait_for(
        || rig.server.pane_opt(&pane, "@agent_kind") == "claude",
        SETTLE
    ));
    let listed = rig
        .server
        .ok(&["agent", "list"], "cli:agent:list", "agent_list");
    let agent: Value = listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["terminal_id"] == pane.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        agent["sandbox"],
        json!({"profile": "box", "runtime": "docker"}),
        "{listed}"
    );

    rig.stop();
    assert!(wait_for(
        || rig.server.pane_opt(&pane, "@agent_container").is_empty(),
        SETTLE
    ));
    assert!(wait_for(
        || rig.server.pane_opt(&pane, "@agent_kind").is_empty(),
        SETTLE
    ));
}

/// Two tmux servers whose sockets have one name in two folders (the `default` of two TMUX_TMPDIRs,
/// two `-S` sockets) are two servers, which number their panes alike: the report folder taw keeps
/// for the first pane of one is not the folder of the first pane of the other. What `taw sandbox`
/// asks of its pane and what the pane's watcher asks of its server name the folder alike.
#[test]
fn two_servers_whose_sockets_share_a_name_in_two_folders_keep_their_report_folders_apart() {
    use taw::sandbox::report;
    use taw::tmux::{Socket, Tmux};
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let (config, state) = (root.path().join("config"), root.path().join("state"));
    let paths = taw::paths::Paths {
        config_file: config.join("config.toml"),
        override_dir: config.join("agent-detection"),
        remote_manifest_dir: state.join("agent-detection/remote"),
        manifest_marker: state.join("agent-detection/last-check"),
        sessions_dir: state.join("sessions"),
        plugins_dir: state.join("plugins"),
        plugin_state_dir: state.join("plugins/state"),
        logs_dir: state.join("logs"),
        config_dir: config,
        state_dir: state,
    };
    let home = common::shell_env::ShellHome::new();
    let servers: Vec<Tmux> = ["one", "other"]
        .iter()
        .map(|folder| {
            let folder = base.join(folder);
            std::fs::create_dir_all(&folder).unwrap();
            Tmux {
                socket: Some(Socket::Path(folder.join("taw-s5-one-name"))),
                ..Tmux::with_socket_name("taw-unused")
            }
        })
        .collect();
    // Ended before the home takes their socket files away (the home drops after this).
    let _ended = Ended(&servers);
    for server in &servers {
        home.start_server(server, &["-d", "-s", "one", "sleep", "600"]);
    }

    let mut panes = Vec::new();
    let mut folders = Vec::new();
    for server in &servers {
        let pane = server
            .run(&["display-message", "-p", "-t", "one", "#{pane_id}"])
            .unwrap();
        let asked = report::ask(server, &pane)
            .unwrap()
            .expect("tmux knows the pane");
        assert_eq!(
            asked.server,
            report::server_key(server).unwrap(),
            "the start and the watcher name the server alike"
        );
        folders.push(report::folder(&paths, &asked.server, &pane).unwrap());
        panes.push(pane);
    }

    assert_eq!(panes[0], panes[1], "both servers number their panes alike");
    assert_ne!(folders[0], folders[1]);
}

/// Ends each of the servers it holds when it is dropped.
struct Ended<'a>(&'a [taw::tmux::Tmux]);

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        for server in self.0 {
            let _ = server.run(&["kill-server"]);
        }
    }
}

#[test]
fn a_report_the_last_agent_left_in_the_panes_folder_is_gone_before_the_next_start() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    let report = rig.report(&pane);
    std::fs::create_dir_all(&report).unwrap();
    std::fs::write(
        report.join("session.json"),
        "{\"hook_event_name\":\"SessionStart\",\"session_id\":\"the-last-one\"}",
    )
    .unwrap();

    rig.sandbox(&pane, &["--profile", "box"]);

    assert!(rig.wait_for_command(&pane, "docker"));
    assert_eq!(std::fs::read_dir(&report).unwrap().count(), 0);
    rig.stop();
}

#[test]
fn a_profile_that_is_not_there_or_an_agent_taw_does_not_know_starts_nothing_and_says_why() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();

    rig.sandbox(&pane, &["claude", "--profile", "team"]);
    let dir = rig.server.config_dir().join("sandbox");
    assert!(
        rig.screen_holds(
            &pane,
            &format!(
                "taw sandbox: no profile called team (yours: default, box, in {}/)",
                dir.display()
            )
        ),
        "{}",
        rig.server.capture(&pane)
    );
    rig.sandbox(&pane, &["claud"]);
    assert!(rig.screen_holds(
        &pane,
        "taw sandbox: taw knows no agent called claud (taw manifest list names them)"
    ));

    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "");
}

#[test]
fn a_profile_that_keeps_a_shell_in_front_gets_no_agent_row_by_design() {
    let rig = Rig::new();
    rig.profile(
        "shellfront",
        "#!/bin/sh\n'{docker}' run --rm -it box-image \"$@\"\necho after\n",
    );
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["claude", "--profile", "shellfront"]);

    assert!(wait_for(|| rig.fake.runs().len() == 1, SETTLE));
    // The typed line read, and nothing of it left to read: the next read after output is the drawn
    // screen's.
    assert!(
        read_all_output(&rig, &pane, 0),
        "{}",
        rig.server.watcher_logs()
    );
    let seen = reads_after_output(&rig, &pane);
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    // The read the drawn screen brought, not one the watcher makes anyway, and what it decided
    // written: what the watcher makes of Claude's screen is there to see.
    assert!(
        read_all_output(&rig, &pane, seen),
        "{}",
        rig.server.watcher_logs()
    );
    // The profile's own interpreter is in front: tmux names it bash on macOS, whose /bin/sh runs
    // bash, and sh or dash elsewhere.
    let command = rig.server.display(&pane, "#{pane_current_command}");
    assert!(
        matches!(command.as_str(), "sh" | "bash" | "dash"),
        "{command}"
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_kind"), "");
    rig.stop();
}

/// The profile's mark goes when the container leaves the pane, every option of it, and the pane's
/// report folder is emptied with it: a later agent in that pane is no sandboxed one.
#[test]
fn the_profile_mark_goes_when_the_container_leaves_the_pane() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    std::fs::write(rig.report(&pane).join("leftover"), "x").unwrap();

    rig.stop();

    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_sandbox_pid"),
        "",
        "the mark's process goes with it"
    );
    assert!(
        wait_for(
            || std::fs::read_dir(rig.report(&pane))
                .is_ok_and(|mut entries| entries.next().is_none()),
            SETTLE
        ),
        "the folder is emptied when the agent leaves: {}",
        rig.server.watcher_logs()
    );
}

/// A profile that ends at once, before it starts a container, does not leave the
/// pane marked. (An earlier rule kept this mark: no read saw the profile at work.)
#[test]
fn a_profile_that_ends_at_once_takes_its_mark_with_it() {
    let rig = Rig::new();
    rig.profile(
        "fails",
        "#!/bin/sh\necho 'taw sandbox fails: docker did not answer' >&2\nexit 1\n",
    );
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "fails"]);

    assert!(rig.screen_holds(&pane, "taw sandbox fails: docker did not answer"));
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox_pid"), "");
    let got = rig
        .server
        .ok(&["pane", "get", &pane], "cli:pane:get", "pane");
    assert_eq!(got["pane"]["sandbox"], Value::Null, "{got}");
}

/// A profile that ends without a word, in a shell whose prompt prints nothing, is seen ending all
/// the same: a marked pane whose profile still runs is read again every second, output or not.
#[test]
fn a_profile_that_ends_without_a_word_is_still_seen_ending() {
    let rig = Rig::new();
    rig.profile(
        "quiet",
        "#!/bin/sh\necho 'quiet: starting'\nsleep 2\nexit 1\n",
    );
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // A prompt that prints nothing: once the profile has ended, the pane prints nothing more.
    rig.type_line(&pane, "PS1=''");

    rig.sandbox(&pane, &["--profile", "quiet"]);

    assert!(
        rig.screen_holds(&pane, "quiet: starting"),
        "{}",
        rig.server.capture(&pane)
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_sandbox"),
        "quiet",
        "marked while it runs"
    );
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
}

/// A profile whose process this user may not signal (one that became `sudo ...`, here pid 1, which
/// is root's) is a process that runs: its mark is kept through every read, and is never taken for a
/// profile that has ended.
#[test]
fn a_mark_whose_process_may_not_be_signalled_is_kept() {
    let rig = Rig::new();
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.server.set_pane_opt(&pane, "@agent_sandbox", "box");
    rig.server.set_pane_opt(&pane, "@agent_sandbox_pid", "1");

    for _ in 1..=2 {
        let seen = reads_after_output(&rig, &pane);
        rig.type_line(&pane, "true");
        assert!(
            read_all_output(&rig, &pane, seen),
            "{}",
            rig.server.watcher_logs()
        );
    }

    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox_pid"), "1");
}

/// A restore marks the pane with the line it types, and no process: a pane whose shell has run no
/// profile keeps the mark however long it is idle, through every read.
#[test]
fn a_mark_with_no_process_is_kept() {
    let rig = Rig::new();
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.server.set_pane_opt(&pane, "@agent_sandbox", "default");

    // Each line typed is output, and a read after it: each was a chance to take the mark off, and
    // what each read decided is written before the next line is typed.
    for _ in 1..=2 {
        let seen = reads_after_output(&rig, &pane);
        rig.type_line(&pane, "true");
        assert!(
            read_all_output(&rig, &pane, seen),
            "{}",
            rig.server.watcher_logs()
        );
    }

    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "default");
}

/// A server whose config hooks the two sandbox events, each writing its `TAW_*` variables to a
/// file (written beside it and renamed into place, so that a file that is there is whole), and a
/// line for each run of either to a log.
fn hooked(rig: &Rig) -> (PathBuf, PathBuf, PathBuf) {
    let (started, stopped, runs) = (
        rig.scratch.join("started.env"),
        rig.scratch.join("stopped.env"),
        rig.scratch.join("sandbox-hooks.log"),
    );
    let hook = |event: &str, file: &Path| {
        format!(
            "\"{event}\" = \"echo {event} >> {runs}; env | grep '^TAW_' | sort > {part} && mv {part} {file}\"",
            runs = quoted(&runs.display().to_string()),
            part = quoted(&file.with_extension("env.part").display().to_string()),
            file = quoted(&file.display().to_string()),
        )
    };
    rig.server.write_config(&format!(
        "{CONFIG}\n[hooks]\n{}\n{}\n",
        hook("sandbox.started", &started),
        hook("sandbox.stopped", &stopped)
    ));
    (started, stopped, runs)
}

/// The variables a hook wrote with `env | sort`, by name.
fn env_of(file: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(name, value)| (name.to_string(), value.to_string()))
        })
        .collect()
}

/// How many times the watchers of `pane` fired `event`, as their log says it when they fire it
/// (before the hook has run, which it does in the background).
fn fired(rig: &Rig, pane: &str, event: &str) -> usize {
    std::fs::read_to_string(
        rig.server
            .state_dir()
            .join("logs")
            .join(format!("watch-{pane}.log")),
    )
    .unwrap_or_default()
    .matches(&format!("pane {pane}: {event} ("))
    .count()
}

/// The hooks that ran, by event, in the order they ran.
fn hook_runs(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn sandbox_started_and_stopped_fire_when_the_marker_comes_and_goes() {
    let rig = Rig::new();
    let (started, stopped, runs) = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);

    assert!(
        wait_for(|| started.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    let env = env_of(&started);
    assert_eq!(env["TAW_EVENT"], "sandbox.started");
    assert_eq!(env["TAW_PANE"], pane);
    assert_eq!(env["TAW_AGENT"], "claude");
    assert!(!stopped.exists());

    rig.stop();
    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    let env = env_of(&stopped);
    assert_eq!(env["TAW_EVENT"], "sandbox.stopped");
    assert_eq!(env["TAW_PANE"], pane);
    assert_eq!(env["TAW_AGENT"], "claude");
    assert_eq!(hook_runs(&runs), ["sandbox.started", "sandbox.stopped"]);
}

/// A stand-in for tmux, made in `dir`, for a rig's watchers and every taw the rig runs: once, just
/// before the first `if-shell` call that holds `before` (one of a watcher's guarded writes), it moves
/// the report stamp of the pane that call is about, as a report landing between the watcher's read
/// and its write does; every call then runs the real tmux. Returns the variables a rig starts with
/// for it ([`Rig::with_env`]), and the folder whose making says the stamp was moved.
fn a_tmux_that_moves_the_stamp_once(dir: &Path, before: &str) -> (Vec<(String, String)>, PathBuf) {
    let script = dir.join("tmux-that-moves-the-stamp");
    let moved = dir.join("stamp-moved");
    common::write_script(
        &script,
        "case \"$*\" in\n\
         *if-shell*\"$TAW_TEST_MOVE_STAMP_BEFORE\"*)\n  \
           if mkdir \"$TAW_TEST_MOVED\" 2>/dev/null; then\n    \
             pane=\n    \
             prev=\n    \
             for arg in \"$@\"; do\n      \
               if [ \"$prev\" = -t ] && [ -z \"$pane\" ]; then pane=$arg; fi\n      \
               prev=$arg\n    \
             done\n    \
             \"$TAW_TEST_REAL_TMUX\" \"$1\" \"$2\" set-option -p -t \"$pane\" @agent_report_stamp moved-by-a-report\n  \
           fi\n  \
           ;;\n\
         esac\n\
         exec \"$TAW_TEST_REAL_TMUX\" \"$@\"\n",
    );
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let env = [
        ("TAW_TMUX_BIN", script.display().to_string()),
        ("TAW_TEST_REAL_TMUX", real.display().to_string()),
        ("TAW_TEST_MOVE_STAMP_BEFORE", before.to_string()),
        ("TAW_TEST_MOVED", moved.display().to_string()),
    ]
    .map(|(name, value)| (name.to_string(), value));
    (env.to_vec(), moved)
}

/// A report that lands between the read that finds a container come and the write that marks the
/// pane with it holds that mark back to the next read, which makes it: a mark held back is never
/// taken for one that was made.
#[test]
fn a_container_mark_a_report_held_back_is_made_at_the_next_read() {
    let tmp = tempfile::tempdir().unwrap();
    let (env, moved) = a_tmux_that_moves_the_stamp_once(tmp.path(), "@agent_container docker");
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let rig = Rig::with_env(&env);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);

    assert!(
        wait_for(
            || rig
                .server
                .watcher_logs()
                .contains("the container docker is marked at the next read"),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(moved.exists());
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.pane_opt(&pane, "@agent_container") == "docker"
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    rig.stop();
}

/// A report that lands between the read that finds a profile ended and the write that takes its mark
/// off holds that end back: the pane's report folder keeps what it holds until the end lands, which
/// it does at the read a second later that the end held back is owed, whether or not the pane prints.
#[test]
fn a_mark_end_a_report_held_back_empties_the_folder_only_once_it_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let (env, moved) =
        a_tmux_that_moves_the_stamp_once(tmp.path(), "@agent_sandbox ; set-option -u");
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let rig = Rig::with_env(&env);
    rig.profile(
        "leaves",
        "#!/bin/sh\necho x > \"$TAW_SANDBOX_REPORT/leftover\"\necho 'leaves: ended'\nexit 1\n",
    );
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "leaves"]);

    assert!(rig.screen_holds(&pane, "leaves: ended"));
    let leftover = rig.report(&pane).join("leftover");
    assert!(
        wait_for(
            || rig
                .server
                .watcher_logs()
                .contains("has ended there, and the pane changed since it was read"),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(moved.exists());
    assert!(
        leftover.exists(),
        "the folder was emptied before the end landed"
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "leaves");

    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        wait_for(|| !leftover.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
}

/// A sandbox that ends with its pane (x on its row, a closed window) is a container that has left the
/// pane: `sandbox.stopped` fires once, told what left, though the pane is gone. (A watcher whose pipe
/// is only taken away, as `taw watch detach` and a restart do, fires none: the test after this one.)
#[test]
fn a_sandbox_that_ends_with_its_pane_fires_one_sandbox_stopped() {
    let rig = Rig::new();
    let (started, stopped, runs) = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| started.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );

    // x on its row: the popup's close is `taw pane close`'s kill-pane.
    rig.server
        .ok(&["pane", "close", &pane], "cli:pane:close", "ok");

    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    let env = env_of(&stopped);
    assert_eq!(env["TAW_EVENT"], "sandbox.stopped");
    assert_eq!(env["TAW_PANE"], pane);
    assert_eq!(env["TAW_AGENT"], "claude");
    assert_eq!(env["TAW_SANDBOX"], "box");
    assert_eq!(env["TAW_RUNTIME"], "docker");
    assert_eq!(env["TAW_SESSION"], "one");
    assert_eq!(hook_runs(&runs), ["sandbox.started", "sandbox.stopped"]);
    rig.stop();
}

/// The same when the whole tmux server ends with the container in front of the pane: its watcher's
/// pipe closes, no server answers for the pane, and `sandbox.stopped` fires, told what left.
#[test]
fn a_sandbox_whose_server_ends_fires_sandbox_stopped_too() {
    let rig = Rig::new();
    let (started, stopped, runs) = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| started.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );

    rig.server.tmux_ok(&["kill-server"]);

    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    let env = env_of(&stopped);
    assert_eq!(env["TAW_PANE"], pane);
    assert_eq!(env["TAW_SANDBOX"], "box");
    assert_eq!(env["TAW_RUNTIME"], "docker");
    assert_eq!(hook_runs(&runs), ["sandbox.started", "sandbox.stopped"]);
}

/// A watcher whose pipe is only taken away (`taw watch detach`, as a restart takes it too) ends
/// nothing: the pane and the container in it are still there, and no `sandbox.stopped` fires.
#[test]
fn a_watcher_taken_away_from_a_running_sandbox_fires_no_stop() {
    let rig = Rig::new();
    let (started, stopped, runs) = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| started.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );

    rig.server
        .ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");

    // The watcher has seen its pipe close and ended: what it fires there, it fires before it says so,
    // and a hook it started writes its line within a moment.
    assert!(
        wait_for(
            || rig
                .server
                .watcher_logs()
                .contains(&format!("stopped watching pane {pane}")),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!stopped.exists(), "{}", rig.server.watcher_logs());
    assert_eq!(hook_runs(&runs), ["sandbox.started"]);
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "docker");
    rig.stop();
}

/// A watcher started again while the agent runs fires no second `sandbox.started`,
/// and the pane keeps its marks.
#[test]
fn a_watcher_started_again_announces_no_container_that_was_already_there() {
    let rig = Rig::new();
    let (started, _stopped, runs) = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| started.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );

    rig.server
        .ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
    assert!(wait_for(|| rig.server.piped(&pane) == "0", SETTLE));
    let reads = rig.server.watcher_reads(&pane);
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // The new watcher's first read, then reads after output: each a chance to announce the container
    // again, and what each decided written before the count moves on.
    assert!(
        wait_for(|| rig.server.watcher_reads(&pane) > reads, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    let seen = reads_after_output(&rig, &pane);
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        read_all_output(&rig, &pane, seen),
        "{}",
        rig.server.watcher_logs()
    );

    assert_eq!(
        fired(&rig, &pane, "sandbox.started"),
        1,
        "a second sandbox.started: {}",
        rig.server.watcher_logs()
    );
    assert_eq!(hook_runs(&runs), ["sandbox.started"]);
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "docker");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    rig.stop();
}

/// A `ps` first on the watchers' `PATH` of a rig, which refuses while its file is there, and notes
/// each refusal: what the watcher asks it to find the process in front of a pane.
struct Ps {
    broken: PathBuf,
    refused: PathBuf,
}

impl Ps {
    fn refuse(&self) {
        std::fs::write(&self.broken, "").unwrap();
    }

    fn answer(&self) {
        std::fs::remove_file(&self.broken).unwrap();
    }

    fn refusals(&self) -> usize {
        std::fs::read_to_string(&self.refused)
            .unwrap_or_default()
            .lines()
            .count()
    }
}

/// A rig whose watchers ask a [`Ps`] that can refuse, with the sandbox hooks hooked ([`hooked`]), the
/// fake runtime running Claude in a pane through a profile, and its `sandbox.started` fired.
fn sandbox_with_a_ps_that_can_refuse(tmp: &Path) -> (Rig, Ps, String, (PathBuf, PathBuf, PathBuf)) {
    let ps = Ps {
        broken: tmp.join("ps-broken"),
        refused: tmp.join("ps-refused"),
    };
    let ps_dir = tmp.join("fake-ps");
    common::write_script(
        &ps_dir.join("ps"),
        &format!(
            "if [ -e {broken} ]; then echo refused >> {refused}; exit 1; fi\nexec /bin/ps \"$@\"\n",
            broken = quoted(&ps.broken.display().to_string()),
            refused = quoted(&ps.refused.display().to_string()),
        ),
    );
    // Since the system read, only the ps fallback can fail the way this `ps` breaks it.
    let rig = Rig::with_first_on_the_watchers_path(&ps_dir, &[("TAW_FOREGROUND_BY_PS", "1")]);
    let hooks = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| hooks.0.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    (rig, ps, pane, hooks)
}

/// A moment in which the watcher cannot look into the runtime in front of the pane (a `ps` that
/// failed under load) is no container leaving the pane and none coming: no `sandbox.stopped` and no
/// second `sandbox.started` fire, and the pane keeps its runtime and its profile's mark.
#[test]
fn a_moment_the_watcher_cannot_look_into_the_runtime_is_no_container_leaving() {
    let tmp = tempfile::tempdir().unwrap();
    let (rig, ps, pane, (_started, stopped, runs)) = sandbox_with_a_ps_that_can_refuse(tmp.path());

    // `ps` refuses through reads that what the pane prints owes, until it has refused the watcher
    // twice: the process in front is asked for at most once a second, and five refusals in a row
    // are a runtime given up on.
    ps.refuse();
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                ps.refusals() >= 2
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    ps.answer();
    let seen = reads_after_output(&rig, &pane);
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        read_all_output(&rig, &pane, seen),
        "{}",
        rig.server.watcher_logs()
    );

    assert_eq!(
        hook_runs(&runs),
        ["sandbox.started"],
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "docker");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    rig.stop();
    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(hook_runs(&runs), ["sandbox.started", "sandbox.stopped"]);
}

/// The same under a failed look: a watcher started again while `ps` refuses cannot look into the
/// runtime at its first read, and takes the pane as it is marked (the runtime, and the agent the pane
/// records in it), as a watcher that had looked before keeps what it found: no `sandbox.stopped`, no
/// second `sandbox.started`, and nothing the agent had is taken off again.
#[test]
fn a_watcher_started_again_while_it_cannot_look_into_the_runtime_announces_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (rig, ps, pane, (_started, _stopped, runs)) = sandbox_with_a_ps_that_can_refuse(tmp.path());
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    rig.server
        .ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
    assert!(wait_for(|| rig.server.piped(&pane) == "0", SETTLE));

    ps.refuse();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // The new watcher's first reads, while `ps` refuses it: a look that failed is two refusals.
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                ps.refusals() >= 2
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let seen = reads_after_output(&rig, &pane);
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        read_all_output(&rig, &pane, seen),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "docker");
    ps.answer();
    let seen = reads_after_output(&rig, &pane);
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        read_all_output(&rig, &pane, seen),
        "{}",
        rig.server.watcher_logs()
    );

    assert_eq!(
        hook_runs(&runs),
        ["sandbox.started"],
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(fired(&rig, &pane, "sandbox.started"), 1);
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "docker");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    rig.stop();
}

/// A runtime the watcher has given up on looking into (five refusals of `ps` in a row, a second apart
/// at the least) is taken as running no agent, as it was before a look kept one: the pane's runtime
/// goes, with `sandbox.stopped`, and comes back, with `sandbox.started`, once the runtime can be read
/// again. The profile's mark stays throughout, since the process that set it still runs.
#[test]
fn a_runtime_the_watcher_gives_up_on_is_taken_as_running_no_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let (rig, ps, pane, (_started, stopped, runs)) = sandbox_with_a_ps_that_can_refuse(tmp.path());

    ps.refuse();
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.pane_opt(&pane, "@agent_container").is_empty()
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        rig.server
            .watcher_logs()
            .contains("could not read what docker was started with"),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(ps.refusals() >= 5, "{}", ps.refusals());
    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");

    ps.answer();
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.pane_opt(&pane, "@agent_container") == "docker"
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        wait_for(
            || hook_runs(&runs) == ["sandbox.started", "sandbox.stopped", "sandbox.started"],
            SETTLE
        ),
        "{:?}",
        hook_runs(&runs)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    rig.stop();
}

#[test]
fn the_two_sandbox_hooks_are_told_the_profile_and_the_runtime() {
    let rig = Rig::new();
    let (started, stopped, _runs) = hooked(&rig);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| started.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    rig.stop();
    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );

    for file in [&started, &stopped] {
        let env = env_of(file);
        assert_eq!(env["TAW_SANDBOX"], "box", "{}", file.display());
        assert_eq!(env["TAW_RUNTIME"], "docker", "{}", file.display());
    }
}

/// A sandboxed agent's state comes from its screen, read from outside the container, and its turn
/// that ends with nobody looking is a done toast, as any agent's is; the notification's hook is told
/// the profile and the runtime.
#[test]
fn a_sandboxed_agents_done_toast_fires_as_any_agents_and_its_hook_is_told_where_it_ran() {
    let tmp = tempfile::tempdir().unwrap();
    let recorder = common::NotifyRecorder::new(tmp.path());
    let told = tmp.path().join("notification.env");
    let rig = Rig::with_env(&[("TAW_NOTIFY_CMD", &recorder.script.display().to_string())]);
    rig.server.write_config(&format!(
        "[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"system\"\ndelay_seconds = 0\ndone_delay_seconds = 0\n\n[ui.sound]\nenabled = false\n\n[hooks]\nnotification = \"env | grep '^TAW_' | sort > {part} && mv {part} {told}\"\n",
        part = quoted(&told.with_extension("env.part").display().to_string()),
        told = quoted(&told.display().to_string()),
    ));
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{273b} Claude Code", CLAUDE_WORKING);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_state") == "working",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);

    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_state") == "done",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        wait_for(|| recorder.matching("done").len() == 1, SETTLE),
        "one done toast: {:?}",
        recorder.lines()
    );
    assert!(wait_for(|| told.exists(), SETTLE));
    let env = env_of(&told);
    assert_eq!(
        (env["TAW_SANDBOX"].as_str(), env["TAW_RUNTIME"].as_str()),
        ("box", "docker"),
        "{env:?}"
    );
    assert_eq!(env["TAW_KIND"], "done", "{env:?}");
    rig.stop();
}

/// The session the pane's last agent reported, and the command it reported that resumes it, are
/// taken off when `taw sandbox` starts in the pane, even with nothing printed between the two:
/// `claude; taw sandbox --profile box` on one line, so that the watcher reads neither the shell nor
/// the profile between them and takes the container's Claude for the same agent. Kept, a save held
/// that session and that command, and a restore typed one of them outside the sandbox.
#[test]
fn the_session_the_last_agent_reported_goes_when_taw_sandbox_starts_and_a_save_holds_none() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // A plain Claude: a program called claude, by its path, that waits until it is woken.
    let plain = rig.scratch.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let claude = common::tmux_named(&plain, "claude");
    let claude = claude.display().to_string();
    rig.type_line(
        &pane,
        &format!(
            "{}; {}",
            line(&[
                claude.as_str(),
                "-L",
                rig.server.socket(),
                "wait-for",
                "s4-plain-claude"
            ]),
            line(&[common::taw_bin(), "sandbox", "--profile", "box"])
        ),
    );
    assert!(rig.wait_for_command(&pane, "claude"));
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    // What its hook reports when it starts a conversation, with the command that resumes it.
    let (code, reported) = rig.server.taw(&[
        "pane",
        "report-agent-session",
        &pane,
        "--source",
        "integration:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        "host-conv-1",
        "--",
        "claude",
        "--resume",
        "host-conv-1",
    ]);
    assert_eq!(code, 0, "{reported}");
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_session_id"),
        "host-conv-1"
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_resume_argv"),
        "claude --resume host-conv-1"
    );
    // The new agent's grace is over, so no read is owed to it; then the plain Claude ends right after
    // a read, so that the next one, the identity recheck five seconds on, comes once the container is
    // in front: nothing prints between the two, and no read sees the shell or the profile.
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_state") == "idle",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let reads = rig.server.watcher_reads(&pane);
    assert!(
        wait_for(|| rig.server.watcher_reads(&pane) > reads, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );

    rig.server.wake("s4-plain-claude");

    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}\n--- screen ---\n{}",
        rig.server.watcher_logs(),
        rig.server.capture(&pane)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    for option in [
        "@agent_session_id",
        "@agent_session_path",
        "@agent_reported_kind",
        "@agent_resume_source",
        "@agent_resume_agent",
        "@agent_resume_argv",
    ] {
        assert_eq!(rig.server.pane_opt(&pane, option), "", "{option}");
    }
    rig.server.ok(
        &["session", "save", "--name", "sandboxed"],
        "cli:session:save",
        "session_saved",
    );
    let saved = std::fs::read_to_string(
        rig.server
            .state_dir()
            .join("sessions")
            .join("sandboxed.json"),
    )
    .unwrap();
    let snapshot: Value = serde_json::from_str(&saved).unwrap();
    let panes: Vec<&Value> = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_array().unwrap())
        .filter(|pane| pane["agent_kind"] == "claude")
        .collect();
    assert_eq!(panes.len(), 1, "{saved}");
    assert_eq!(panes[0]["agent_session_id"], Value::Null, "{saved}");
    assert_eq!(panes[0]["agent_resume"], Value::Null, "{saved}");
    assert!(!saved.contains("host-conv-1"), "{saved}");
    rig.stop();
}

/// The session a plain Claude reported, and the command it reported that resumes it, go when a
/// container of the same kind comes into the pane straight after it, typed by hand (box and the
/// like, no `taw sandbox` and so no profile mark), with nothing printed between the two: the
/// watcher reads neither the shell nor anything else between them, and takes the container's
/// Claude for the same agent. Kept, a save held that session and that command, and a restore typed
/// `claude --resume host-conv-1` outside any container.
#[test]
fn the_session_a_plain_agent_reported_goes_when_a_container_typed_by_hand_comes() {
    let rig = Rig::new();
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // A plain Claude: a program called claude, by its path, that waits until it is woken. Then a
    // container typed by hand on the same line: the fake runtime, by its path, running Claude.
    let plain = rig.scratch.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let claude = common::tmux_named(&plain, "claude").display().to_string();
    let docker = rig.fake.bin.join("docker").display().to_string();
    rig.type_line(
        &pane,
        &format!(
            "{}; {}",
            line(&[
                claude.as_str(),
                "-L",
                rig.server.socket(),
                "wait-for",
                "s5-plain-claude"
            ]),
            line(&[docker.as_str(), "run", "--rm", "-it", "box-image", "claude"])
        ),
    );
    assert!(rig.wait_for_command(&pane, "claude"));
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    // What its hook reports when it starts a conversation, with the command that resumes it.
    let (code, reported) = rig.server.taw(&[
        "pane",
        "report-agent-session",
        &pane,
        "--source",
        "integration:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        "host-conv-1",
        "--",
        "claude",
        "--resume",
        "host-conv-1",
    ]);
    assert_eq!(code, 0, "{reported}");
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_session_id"),
        "host-conv-1"
    );
    // The new agent's grace is over, so no read is owed to it; then the plain Claude ends right after
    // a read, so that the next one, the identity recheck five seconds on, comes once the container is
    // in front: nothing prints between the two, and no read sees the shell.
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_state") == "idle",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let reads = rig.server.watcher_reads(&pane);
    assert!(
        wait_for(|| rig.server.watcher_reads(&pane) > reads, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    // A save made before the container came holds the session: the drop asks for another. Nothing
    // else asks for one between here and the container (no plugin here takes the request).
    let save_asked = rig.server.state_dir().join("sessions/.autosave-requested");
    let _ = std::fs::remove_file(&save_asked);

    rig.server.wake("s5-plain-claude");

    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}\n--- screen ---\n{}",
        rig.server.watcher_logs(),
        rig.server.capture(&pane)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_sandbox"),
        "",
        "typed by hand"
    );
    assert!(
        wait_for(|| save_asked.exists(), SETTLE),
        "no save asked for: {}",
        rig.server.watcher_logs()
    );
    for option in [
        "@agent_session_id",
        "@agent_session_path",
        "@agent_reported_kind",
        "@agent_token_session_start_source",
        "@agent_resume_source",
        "@agent_resume_agent",
        "@agent_resume_argv",
    ] {
        assert_eq!(rig.server.pane_opt(&pane, option), "", "{option}");
    }
    rig.server.ok(
        &["session", "save", "--name", "by-hand"],
        "cli:session:save",
        "session_saved",
    );
    let saved =
        std::fs::read_to_string(rig.server.state_dir().join("sessions").join("by-hand.json"))
            .unwrap();
    let snapshot: Value = serde_json::from_str(&saved).unwrap();
    let panes: Vec<&Value> = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_array().unwrap())
        .filter(|pane| pane["agent_kind"] == "claude")
        .collect();
    assert_eq!(panes.len(), 1, "{saved}");
    assert_eq!(panes[0]["agent_session_id"], Value::Null, "{saved}");
    assert_eq!(panes[0]["agent_resume"], Value::Null, "{saved}");
    assert!(!saved.contains("host-conv-1"), "{saved}");
    rig.stop();
}

/// A save made while a container runtime is in front of the pane, before any watcher has read it and
/// marked the pane with it, keeps the runtime beside the session: the plain Claude before it reported
/// host-conv-1, and a restore resumes a pane saved with a runtime and no profile not at all
/// (`restore::plan_of`), where a session saved without the runtime would be typed outside any
/// container. Here no watcher runs once the container has come (it was detached), so the pane holds
/// the plain agent's session and no `@agent_container`.
#[test]
fn a_save_before_any_read_of_a_container_typed_by_hand_keeps_the_runtime_beside_its_session() {
    let rig = Rig::new();
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    let plain = rig.scratch.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let claude = common::tmux_named(&plain, "claude").display().to_string();
    let docker = rig.fake.bin.join("docker").display().to_string();
    rig.type_line(
        &pane,
        &format!(
            "{}; {}",
            line(&[
                claude.as_str(),
                "-L",
                rig.server.socket(),
                "wait-for",
                "s5-early-claude"
            ]),
            line(&[docker.as_str(), "run", "--rm", "-it", "box-image", "claude"])
        ),
    );
    assert!(rig.wait_for_command(&pane, "claude"));
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let (code, reported) = rig.server.taw(&[
        "pane",
        "report-agent-session",
        &pane,
        "--source",
        "integration:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        "host-conv-1",
        "--",
        "claude",
        "--resume",
        "host-conv-1",
    ]);
    assert_eq!(code, 0, "{reported}");
    rig.server
        .ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
    assert!(wait_for(|| rig.server.piped(&pane) == "0", SETTLE));

    rig.server.wake("s5-early-claude");
    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_container"), "");
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_session_id"),
        "host-conv-1"
    );
    rig.server.ok(
        &["session", "save", "--name", "early"],
        "cli:session:save",
        "session_saved",
    );

    let saved = std::fs::read_to_string(rig.server.state_dir().join("sessions").join("early.json"))
        .unwrap();
    let snapshot: Value = serde_json::from_str(&saved).unwrap();
    let panes: Vec<&Value> = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_array().unwrap())
        .filter(|pane| pane["agent_kind"] == "claude")
        .collect();
    assert_eq!(panes.len(), 1, "{saved}");
    assert_eq!(panes[0]["agent_session_id"], "host-conv-1", "{saved}");
    assert_eq!(panes[0]["agent_container"], "docker", "{saved}");
    assert_eq!(panes[0]["agent_sandbox"], Value::Null, "{saved}");
    rig.stop();
}

#[test]
fn outside_tmux_it_says_so_in_one_line_and_runs_the_profile_with_no_pane() {
    let rig = Rig::new();
    rig.profile("box", BOX);

    let out = rig.outside(&["sandbox", "claude", "--profile", "box"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "taw sandbox: not in tmux, so this agent gets no popup row and no restore\n"
    );
    assert_eq!(
        std::fs::read_to_string(&rig.env_file).unwrap(),
        format!(
            "TAW_SANDBOX_AGENT=claude\nTAW_SANDBOX_DIR={}\nTAW_SANDBOX_PROFILE=box\n",
            rig.scratch.display()
        )
    );
    assert_eq!(
        rig.fake.runs()[0][1..],
        ["run", "--rm", "-it", "box-image", "claude"]
    );
    // No pane, so no report folder and nothing marked.
    assert!(!rig.server.state_dir().join("sandbox/panes").exists());
}

/// taw's own profile, run from a pane: written where taw keeps it, saying what it does first, and
/// its container stopped once the pane is closed, as x on its row closes it. It asks the version of
/// the claude its PATH finds, which here is the net's: that is the one call the net takes.
#[test]
fn taws_own_profile_runs_from_a_pane_and_closing_the_pane_stops_its_container() {
    let rig = Rig::with_the_runtime_by_name();
    let pane = rig.window();

    rig.sandbox(&pane, &[]);

    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "default");
    let written = rig.server.state_dir().join("sandbox/default");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        taw::sandbox::DEFAULT_PROFILE
    );
    assert!(
        wait_for(|| !rig.fake.runs().is_empty(), SETTLE),
        "{:?}",
        rig.fake.calls()
    );
    let run = rig.fake.runs().remove(0);
    let name = run[run.iter().position(|w| w == "--name").unwrap() + 1].clone();
    assert!(
        rig.server
            .capture(&pane)
            .contains("taw sandbox default: claude in docker, in "),
        "{}",
        rig.server.capture(&pane)
    );

    // x on its row: the popup's close is `taw pane close`'s kill-pane.
    rig.server
        .ok(&["pane", "close", &pane], "cli:pane:close", "ok");

    assert!(
        wait_for(
            || rig
                .fake
                .calls()
                .iter()
                .any(|c| c[1..] == ["stop", "-t", "2", name.as_str()]),
            Duration::from_secs(15)
        ),
        "{:?}",
        rig.fake.calls()
    );
    assert_eq!(rig.net.take(), ["claude --version"]);
}

/// A config taw cannot read whole fails closed. With no --profile, taw sandbox cannot tell which
/// profile you chose, a strict one of yours or default, so it starts nothing and says why in one
/// line: a file that does not load, and a line of [sandbox] it drops (a key it does not know). With
/// --profile the flag decides, as it always does, and the profile runs.
#[test]
fn a_config_taw_cannot_read_whole_starts_nothing_unless_a_profile_is_named() {
    const TAIL: &str = ", so taw cannot tell which profile you chose: fix it (taw config check says what is wrong), or name one with --profile\n";
    let rig = Rig::new();
    // Your own default and a strict profile, the one the config means: here both write down what
    // they were given and run the fake.
    rig.profile("default", BOX);
    rig.profile("strict", BOX);
    let file = rig.server.config_dir().join("config.toml");
    for (config, starts) in [
        (
            "[sandbox\nprofile = \"strict\"\n",
            format!("taw sandbox: config {} did not load (", file.display()),
        ),
        (
            "[sandbox]\nprofil = \"strict\"\n",
            format!(
                "taw sandbox: config {} has a line in [sandbox] taw drops (unknown config key: sandbox.profil){TAIL}",
                file.display()
            ),
        ),
    ] {
        rig.server.write_config(config);
        let _ = std::fs::remove_file(&rig.env_file);
        let runs = rig.fake.runs().len();

        let out = rig.outside(&["sandbox", "claude"]);

        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(1), "{config:?}: {stderr}");
        assert_eq!(stderr.lines().count(), 1, "{config:?}: {stderr}");
        assert!(stderr.starts_with(&starts), "{stderr}");
        assert!(stderr.ends_with(TAIL), "{stderr}");
        assert!(!rig.env_file.exists(), "{config:?}: a profile ran");
        assert_eq!(rig.fake.runs().len(), runs, "{config:?}");

        let out = rig.outside(&["sandbox", "claude", "--profile", "strict"]);

        assert!(
            out.status.success(),
            "{config:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            std::fs::read_to_string(&rig.env_file)
                .unwrap()
                .contains("TAW_SANDBOX_PROFILE=strict\n"),
            "{config:?}"
        );
        assert_eq!(rig.fake.runs().len(), runs + 1, "{config:?}");
    }
}

/// TMUX_PANE names the pane's report folder, which taw empties, and the pane it marks, and anything
/// can set it. Typed before `taw sandbox`, a folder of yours (whole, or climbing out of taw's state
/// with ../) and a pane tmux does not know each start nothing and say why: the folder keeps its
/// files and its mode, and nothing is marked or made.
#[test]
fn a_tmux_pane_that_is_no_pane_of_tmuxs_starts_nothing_and_empties_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    let victim = rig.scratch.join("victim");
    std::fs::create_dir_all(victim.join("sub")).unwrap();
    std::fs::write(victim.join("notes.txt"), "keep me").unwrap();
    std::fs::write(victim.join("sub/more.txt"), "keep me too").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
    // From the panes' folder, more ../ than it takes to reach /, then the victim.
    let climb = format!(
        "{}{}",
        "../".repeat(40),
        victim.strip_prefix("/").unwrap().display()
    );
    let not_a_pane = |value: &str| {
        format!(
            "taw sandbox: TMUX_PANE is {value:?}, which is no tmux pane (a % and digits), so nothing was started"
        )
    };
    let cases = [
        (
            victim.display().to_string(),
            not_a_pane(&victim.display().to_string()),
        ),
        (climb.clone(), not_a_pane(&climb)),
        (
            "%9999".to_string(),
            "taw sandbox: tmux knows no pane %9999, so nothing was started".to_string(),
        ),
    ];

    let victim_kept = || {
        std::fs::read_to_string(victim.join("notes.txt")).is_ok_and(|text| text == "keep me")
            && std::fs::read_to_string(victim.join("sub/more.txt"))
                .is_ok_and(|text| text == "keep me too")
            && std::fs::metadata(&victim)
                .is_ok_and(|meta| meta.permissions().mode() & 0o777 == 0o755)
    };
    for (value, said) in &cases {
        rig.type_line(
            &pane,
            &format!(
                "TMUX_PANE={} {}",
                quoted(value),
                line(&[common::taw_bin(), "sandbox", "--profile", "box"])
            ),
        );
        let said_it = rig.screen_holds_joined(&pane, said);
        // What it names first: the folder keeps its files and its mode, whatever was said.
        assert!(victim_kept(), "{value}: the victim was changed");
        assert!(
            said_it,
            "{value}: {}",
            rig.server
                .tmux_ok(&["capture-pane", "-p", "-J", "-t", &pane])
        );
    }
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "");
    assert!(!rig.report(&pane).exists());
    assert!(!rig.report("%9999").exists());
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    assert!(!rig.env_file.exists(), "a profile ran");
}

/// A start not typed at the prompt of the pane TMUX_PANE names marks nothing and takes no session
/// off: one an agent's tool call runs with the pane's TMUX and TMUX_PANE (its input no terminal), one
/// sent to the background with &, and one typed in another pane with this one's TMUX_PANE. The agent
/// in front keeps its session and its pane, and no pane is marked. A start typed at the prompt through
/// a script of the person's own still starts.
#[test]
fn a_start_not_typed_at_the_panes_prompt_marks_nothing_and_takes_no_session_off() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let taw = common::taw_bin();
    // A pane with a plain Claude in front, and the session its hook reported.
    let busy = rig.window();
    let plain = rig.scratch.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let claude = common::tmux_named(&plain, "claude").display().to_string();
    rig.type_line(
        &busy,
        &line(&[
            claude.as_str(),
            "-L",
            rig.server.socket(),
            "wait-for",
            "s4-busy-claude",
        ]),
    );
    assert!(rig.wait_for_command(&busy, "claude"));
    let (code, reported) = rig.server.taw(&[
        "pane",
        "report-agent-session",
        &busy,
        "--source",
        "integration:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        "busy-conv",
    ]);
    assert_eq!(code, 0, "{reported}");
    let refused = |pane: &str, why: &str| {
        format!(
            "taw sandbox: not typed at the prompt of pane {pane} ({why}), so nothing was started: type it at the prompt of the pane it should take over (agents and scripts start one with taw agent start --sandbox)"
        )
    };

    // An agent's tool call, with the pane's TMUX and TMUX_PANE and no terminal. A fake that ran
    // would end at once.
    let tmux = format!(
        "{},{},0",
        rig.server.display(&busy, "#{socket_path}"),
        rig.server.display(&busy, "#{pid}")
    );
    let ended = rig.scratch.join("ended");
    std::fs::write(&ended, "").unwrap();
    let (code, _, err) = rig.server.taw_raw_with_env(
        &["sandbox", "--profile", "box"],
        &[
            ("TMUX", tmux.as_str()),
            ("TMUX_PANE", busy.as_str()),
            ("FAKE_RUNTIME_STOP", ended.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 1, "{err}");
    assert_eq!(
        err,
        format!("{}\n", refused(&busy, "its input is no terminal"))
    );

    // Sent to the background with &, at the prompt of another pane, and typed there with the busy
    // pane's TMUX_PANE.
    let idle = rig.window();
    rig.type_line(
        &idle,
        &format!("{} &", line(&[taw, "sandbox", "--profile", "box"])),
    );
    assert!(
        rig.screen_holds_joined(
            &idle,
            &refused(&idle, "it is not the job in front of that pane")
        ),
        "{}",
        rig.server
            .tmux_ok(&["capture-pane", "-p", "-J", "-t", &idle])
    );
    rig.type_line(
        &idle,
        &format!(
            "TMUX_PANE={busy} {}",
            line(&[taw, "sandbox", "--profile", "box"])
        ),
    );
    assert!(
        rig.screen_holds_joined(&idle, &refused(&busy, "its terminal is not that pane's")),
        "{}",
        rig.server
            .tmux_ok(&["capture-pane", "-p", "-J", "-t", &idle])
    );

    for pane in [&busy, &idle] {
        assert_eq!(rig.server.pane_opt(pane, "@agent_sandbox"), "", "{pane}");
        assert!(!rig.report(pane).exists(), "{pane}");
    }
    assert_eq!(rig.server.pane_opt(&busy, "@agent_session_id"), "busy-conv");
    assert_eq!(
        rig.server.display(&busy, "#{pane_current_command}"),
        "claude"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    assert!(!rig.env_file.exists(), "a profile ran");

    // At the prompt, through a script of the person's own: it starts, and marks its own pane.
    let script = rig.scratch.join("my-box");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n{}\n",
            line(&[taw, "sandbox", "--profile", "box"])
        ),
    )
    .unwrap();
    crate::common::make_executable(&script);
    rig.type_line(&idle, &quoted(&script.display().to_string()));
    assert!(
        wait_for(|| rig.fake.runs().len() == 1, SETTLE),
        "{}",
        rig.server.capture(&idle)
    );
    assert_eq!(rig.server.pane_opt(&idle, "@agent_sandbox"), "box");
    rig.stop();
    rig.server.wake("s4-busy-claude");
}

/// A config.toml that is a link to a file that is gone (stow, after the dotfiles moved) is no config
/// taw can read: with no --profile, taw sandbox cannot tell which profile you chose, so it starts
/// nothing and says so in one line; with --profile, the flag decides and the profile runs.
#[test]
fn a_config_that_is_a_link_to_a_file_that_is_gone_starts_nothing_unless_a_profile_is_named() {
    let rig = Rig::new();
    rig.profile("default", BOX);
    rig.profile("strict", BOX);
    let file = rig.server.config_dir().join("config.toml");
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(rig.scratch.join("dotfiles-moved/config.toml"), &file).unwrap();

    let out = rig.outside(&["sandbox", "claude"]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "taw sandbox: config {} is a link to a file that is not there, so taw cannot tell which profile you chose: fix it (taw config check says what is wrong), or name one with --profile\n",
            file.display()
        )
    );
    assert!(!rig.env_file.exists(), "a profile ran");
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());

    let out = rig.outside(&["sandbox", "claude", "--profile", "strict"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::read_to_string(&rig.env_file)
            .unwrap()
            .contains("TAW_SANDBOX_PROFILE=strict\n")
    );
}

/// A session reported from outside the container after the watcher has seen the agent in it (a late
/// report of the agent that left the pane, or anything else that knows the pane's id), with the
/// command that resumes it, lands on the pane, and a save keeps it only with the profile and the
/// runtime beside it: a restore types it through `taw sandbox` and its profile, or not at all, and
/// never outside the sandbox (restore's own tests).
#[test]
fn a_session_reported_onto_a_sandboxed_pane_is_saved_with_its_profile_and_runtime() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let (code, reported) = rig.server.taw(&[
        "pane",
        "report-agent-session",
        &pane,
        "--source",
        "integration:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        "late-conv",
        "--",
        "claude",
        "--resume",
        "late-conv",
    ]);
    assert_eq!(code, 0, "{reported}");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), "late-conv");
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_resume_argv"),
        "claude --resume late-conv"
    );

    rig.server.ok(
        &["session", "save", "--name", "sandboxed"],
        "cli:session:save",
        "session_saved",
    );

    let saved = std::fs::read_to_string(
        rig.server
            .state_dir()
            .join("sessions")
            .join("sandboxed.json"),
    )
    .unwrap();
    let snapshot: Value = serde_json::from_str(&saved).unwrap();
    let panes: Vec<&Value> = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_array().unwrap())
        .filter(|pane| pane["agent_kind"] == "claude")
        .collect();
    assert_eq!(panes.len(), 1, "{saved}");
    assert_eq!(panes[0]["agent_session_id"], "late-conv", "{saved}");
    assert_eq!(panes[0]["agent_sandbox"], "box", "{saved}");
    assert_eq!(panes[0]["agent_container"], "docker", "{saved}");
    assert_eq!(
        panes[0]["agent_resume"]["argv"],
        json!(["claude", "--resume", "late-conv"]),
        "{saved}"
    );
    rig.stop();
}

/// The same for a container typed by hand, which leaves no profile mark (box and the like): a
/// session reported from outside after the watcher has seen the agent in the container (a late
/// report of the plain agent that left the pane) lands on the pane, and a save keeps it with the
/// runtime beside it and no profile. A pane saved so is never resumed (restore's own tests): a
/// container's agent is never resumed by a plain `claude --resume` typed outside.
#[test]
fn a_session_reported_onto_a_pane_whose_container_was_typed_by_hand_is_saved_with_its_runtime() {
    let rig = Rig::new();
    let pane = rig.window();
    let docker = rig.fake.bin.join("docker").display().to_string();
    rig.type_line(
        &pane,
        &line(&[docker.as_str(), "run", "--rm", "-it", "box-image", "claude"]),
    );
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_sandbox"),
        "",
        "typed by hand"
    );
    let (code, reported) = rig.server.taw(&[
        "pane",
        "report-agent-session",
        &pane,
        "--source",
        "integration:claude",
        "--agent",
        "claude",
        "--agent-session-id",
        "late-conv",
        "--",
        "claude",
        "--resume",
        "late-conv",
    ]);
    assert_eq!(code, 0, "{reported}");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), "late-conv");
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_resume_argv"),
        "claude --resume late-conv"
    );

    rig.server.ok(
        &["session", "save", "--name", "by-hand"],
        "cli:session:save",
        "session_saved",
    );

    let saved =
        std::fs::read_to_string(rig.server.state_dir().join("sessions").join("by-hand.json"))
            .unwrap();
    let snapshot: Value = serde_json::from_str(&saved).unwrap();
    let panes: Vec<&Value> = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_array().unwrap())
        .filter(|pane| pane["agent_kind"] == "claude")
        .collect();
    assert_eq!(panes.len(), 1, "{saved}");
    assert_eq!(panes[0]["agent_session_id"], "late-conv", "{saved}");
    assert_eq!(panes[0]["agent_container"], "docker", "{saved}");
    assert_eq!(panes[0]["agent_sandbox"], Value::Null, "{saved}");
    rig.stop();
}

/// A start with TMUX and no TMUX_PANE (a popup's command, a hook's run-shell) is in tmux but in no
/// pane, and says so; one with TMUX_PANE and no TMUX is in no tmux at all. Each runs the profile with
/// no pane, and marks none.
#[test]
fn tmux_without_a_pane_and_a_pane_without_tmux_run_the_profile_with_no_pane() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    let tmux = format!(
        "{},{},0",
        rig.server.display(&pane, "#{socket_path}"),
        rig.server.display(&pane, "#{pid}")
    );
    let no_pane = format!(
        "TAW_SANDBOX_AGENT=claude\nTAW_SANDBOX_DIR={}\nTAW_SANDBOX_PROFILE=box\n",
        rig.scratch.display()
    );
    for (extra, said) in [
        (
            vec![("TMUX", tmux.as_str())],
            "taw sandbox: not in a tmux pane, so this agent gets no popup row and no restore\n",
        ),
        (
            vec![("TMUX_PANE", pane.as_str())],
            "taw sandbox: not in tmux, so this agent gets no popup row and no restore\n",
        ),
    ] {
        let _ = std::fs::remove_file(&rig.env_file);

        let out = rig.outside_with(&["sandbox", "claude", "--profile", "box"], &extra);

        assert!(
            out.status.success(),
            "{extra:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stderr), said, "{extra:?}");
        assert_eq!(
            std::fs::read_to_string(&rig.env_file).unwrap(),
            no_pane,
            "{extra:?}"
        );
    }
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "");
    assert!(!rig.report(&pane).exists());
}

/// A profile of yours whose #! line names a program that is not there is refused in one line that
/// names it, before the line about tmux and before anything runs: the system's own refusal would
/// say "No such file or directory" of the profile, which is there.
#[test]
fn a_profile_whose_interpreter_is_not_there_is_refused_in_one_line_naming_it() {
    let rig = Rig::new();
    let gone = rig.profile(
        "gone",
        "#!/no/such/bash\nexec '{docker}' run --rm -it box-image \"$@\"\n",
    );

    let out = rig.outside(&["sandbox", "claude", "--profile", "gone"]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "taw sandbox: the #! line of the profile gone names /no/such/bash, which is no program you can run (head -1 {})\n",
            gone.display()
        )
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// A profile the system will not run once the pane is marked takes the mark off again, and says
/// why in one line that names the profile with ~ for the home. Here the tmux taw runs takes the
/// profile's x bit away as it marks the pane, past every look taw makes first.
#[test]
fn a_profile_the_system_will_not_run_takes_the_panes_mark_off_again() {
    let rig = Rig::new();
    let config = rig.server.home_dir().join(".config/taw");
    let denied = config.join("sandbox/denied");
    std::fs::create_dir_all(denied.parent().unwrap()).unwrap();
    std::fs::write(&denied, BOX).unwrap();
    crate::common::make_executable(&denied);
    let tmux = rig.scratch.join("tmux-that-shuts-the-profile");
    std::fs::write(
        &tmux,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" @agent_sandbox_pid \"*) chmod 644 {} ;;\nesac\nexec {} \"$@\"\n",
            quoted(denied.to_str().unwrap()),
            quoted(
                taw::tmux::Tmux::with_socket_name("unused")
                    .bin
                    .to_str()
                    .unwrap()
            ),
        ),
    )
    .unwrap();
    crate::common::make_executable(&tmux);
    let pane = rig.window();

    rig.type_line(
        &pane,
        &format!(
            "TAW_CONFIG_DIR={} TAW_TMUX_BIN={} {}",
            quoted(config.to_str().unwrap()),
            quoted(tmux.to_str().unwrap()),
            line(&[common::taw_bin(), "sandbox", "--profile", "denied"]),
        ),
    );

    assert!(
        rig.screen_holds_joined(
            &pane,
            "taw sandbox: could not run the profile ~/.config/taw/sandbox/denied: Permission denied (os error 13)"
        ),
        "{}",
        rig.server
            .tmux_ok(&["capture-pane", "-p", "-J", "-t", &pane])
    );
    for option in ["@agent_sandbox", "@agent_sandbox_pid"] {
        assert_eq!(rig.server.pane_opt(&pane, option), "", "{option}");
    }
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// taw's own profile is written before the pane is marked: a write that fails leaves no mark.
#[test]
fn taws_own_profile_that_cannot_be_written_leaves_no_mark() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.server.state_dir().join("sandbox/default/in-the-way")).unwrap();
    let pane = rig.window();

    rig.sandbox(&pane, &[]);

    assert!(
        rig.screen_holds_joined(
            &pane,
            "taw sandbox: could not write taw's own profile, so nothing was started"
        ),
        "{}",
        rig.server
            .tmux_ok(&["capture-pane", "-p", "-J", "-t", &pane])
    );
    for option in ["@agent_sandbox", "@agent_sandbox_pid"] {
        assert_eq!(rig.server.pane_opt(&pane, option), "", "{option}");
    }
    assert!(!rig.report(&pane).exists());
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// A placement flag is refused before anything runs, with the usage line and a tip that says
/// where taw sandbox runs and how to make the place first: clap's own tip would put the flag after
/// --, where it goes to the agent.
#[test]
fn a_placement_flag_is_refused_with_how_to_make_the_place_first() {
    let rig = Rig::new();
    rig.profile("box", BOX);

    let out = rig.outside(&["sandbox", "--profile", "box", "--split"]);

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: unexpected argument '--split' found\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "\n  tip: taw sandbox runs the agent in the pane it is typed in: make the place first (prefix % or prefix \" for a pane beside this one, prefix c for a window, the popup's n or w for a session or a worktree), then type taw sandbox there\n"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("-- --split"), "{stderr}");
    assert!(stderr.contains("\nUsage: taw sandbox "), "{stderr}");
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// A profile only others may run is refused before anything runs, outside tmux too, in one line
/// that names it with ~ for the home: the system's own refusal would come after the line about
/// tmux.
#[test]
fn a_profile_only_others_may_run_is_refused_in_one_line_with_a_tilde_for_the_home() {
    let rig = Rig::new();
    let config = rig.scratch.join("home/.config/taw");
    let denied = config.join("sandbox/denied");
    std::fs::create_dir_all(denied.parent().unwrap()).unwrap();
    std::fs::write(&denied, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o655)).unwrap();

    let out = rig.outside_with(
        &["sandbox", "claude", "--profile", "denied"],
        &[("TAW_CONFIG_DIR", config.to_str().unwrap())],
    );

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "taw sandbox: the profile denied is not executable (chmod +x ~/.config/taw/sandbox/denied)\n"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

#[test]
fn agent_start_sandbox_waits_for_the_agent_in_its_container_and_names_it() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .tmux_ok(&["set-option", "-p", "-t", &pane, "@agent_kind_hint", "codex"]);

    let started = rig.server.ok(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--sandbox",
            "--profile",
            "box",
            "--timeout",
            "60000",
            "--",
            "--model",
            "opus",
        ],
        "cli:agent:start",
        "agent",
    );

    assert_eq!(started["agent"]["name"], "rev", "{started}");
    assert_eq!(started["agent"]["agent"], "claude", "{started}");
    assert_eq!(
        started["agent"]["sandbox"],
        json!({"profile": "box", "runtime": "docker"}),
        "{started}"
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_kind_hint"),
        "",
        "a sandboxed agent has no hint"
    );
    assert!(
        rig.server
            .capture(&pane)
            .contains("sandbox claude --profile box -- --model opus"),
        "{}",
        rig.server.capture(&pane)
    );
    // The fake writes down its run as it starts, which can be a moment after the start has read
    // the agent off its arguments.
    assert!(
        wait_for(|| !rig.fake.runs().is_empty(), SETTLE),
        "{:?}",
        rig.fake.calls()
    );
    let run = &rig.fake.runs()[0];
    assert_eq!(run[run.len() - 3..], ["claude", "--model", "opus"]);
    rig.stop();
}

#[test]
fn a_profile_that_never_runs_a_runtime_times_out_with_its_message() {
    let rig = Rig::new();
    rig.profile("nothing", "#!/bin/sh\nsleep 30\n");
    let pane = rig.window();

    let (code, env) = rig.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "nothing",
        "--timeout",
        "4000",
    ]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "timeout", "{env}");
    assert_eq!(
        env["error"]["message"],
        format!(
            "taw saw no agent in pane {pane}: a profile must end by running the runtime, with the agent as the container's program"
        )
    );
}

/// Through `agent start`: a profile that ends at once is a start that did not happen,
/// and the pane keeps no mark of it, though no watcher reads the pane.
#[test]
fn a_profile_that_ends_at_once_is_not_started_and_leaves_no_mark() {
    let rig = Rig::new();
    rig.profile(
        "fails",
        "#!/bin/sh\necho 'taw sandbox fails: no recipe for claude' >&2\nexit 1\n",
    );
    let pane = rig.window();

    let (code, env) = rig.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "fails",
        "--timeout",
        "20000",
    ]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "agent_not_started", "{env}");
    assert_eq!(
        env["error"]["message"],
        format!(
            "claude did not start in its sandbox in pane {pane}: its shell is back at its prompt. `taw pane read {pane}` shows what the profile said."
        )
    );
    assert!(
        rig.server
            .capture(&pane)
            .contains("taw sandbox fails: no recipe for claude"),
        "{}",
        rig.server.capture(&pane)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox_pid"), "");
}

#[test]
fn a_profile_that_is_not_there_is_refused_before_anything_is_typed() {
    let rig = Rig::new();
    let pane = rig.window();

    let (code, env) = rig.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "team",
    ]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert!(
        !rig.server.capture(&pane).contains("sandbox"),
        "{}",
        rig.server.capture(&pane)
    );
}

/// A shell that asks a question before its prompt is typed nothing. The question
/// comes from a program the pane's shell runs, as mise's "Trust them?" does (`tests/e2e_agent_start.rs`'
/// `Asking`): a shell that asked it itself, with its own `read`, would be a shell at its prompt.
#[test]
fn a_shell_that_asks_a_question_first_is_typed_nothing() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let ask = rig.scratch.join("ask");
    std::fs::write(
        &ask,
        "#!/bin/sh\nprintf 'Trust them? Yes No All '\nread answer\n",
    )
    .unwrap();
    crate::common::make_executable(&ask);
    let pane = rig.server.tmux_ok(&[
        "new-window",
        "-t",
        "one",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "export PATH='{}'; '{}'; exec /bin/sh",
            rig.path,
            ask.display()
        ),
    ]);
    assert!(
        rig.screen_holds(&pane, "Trust them?"),
        "{}",
        rig.server.capture(&pane)
    );

    let (code, env) = rig.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "box",
        "--timeout",
        "3000",
    ]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "shell_not_ready", "{env}");
    assert!(
        !rig.server.capture(&pane).contains("sandbox"),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn taw_on_the_panes_path_is_typed_as_taw() {
    let tmp = tempfile::tempdir().unwrap();
    let links = std::fs::canonicalize(tmp.path()).unwrap().join("links");
    std::fs::create_dir_all(&links).unwrap();
    std::os::unix::fs::symlink(common::taw_bin(), links.join("taw")).unwrap();
    let rig = Rig::with_on_the_path(&links);
    rig.profile("box", BOX);
    let pane = rig.window();

    rig.server.ok(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--sandbox",
            "--profile",
            "box",
            "--timeout",
            "60000",
        ],
        "cli:agent:start",
        "agent",
    );

    let screen = rig.server.capture(&pane);
    assert!(
        screen.lines().any(
            |row| row.trim_end().ends_with("taw sandbox claude --profile box")
                && !row.contains('/')
        ),
        "{screen}"
    );
    rig.stop();
}

/// The row of `rig`'s `pane` that holds the typed `sandbox claude --profile box`, wrapped lines
/// joined, up to where `sandbox` begins: the word taw is typed as, after the prompt.
fn typed_taw(rig: &Rig, pane: &str) -> String {
    assert!(
        rig.screen_holds_joined(pane, " sandbox claude --profile box"),
        "{}",
        rig.server.capture(pane)
    );
    let screen = rig
        .server
        .tmux_ok(&["capture-pane", "-p", "-J", "-t", pane]);
    let row = screen
        .lines()
        .find(|row| row.contains(" sandbox claude --profile box"))
        .unwrap()
        .to_string();
    let before = row.split(" sandbox claude").next().unwrap();
    before.split_whitespace().last().unwrap().to_string()
}

/// `taw agent start rev --sandbox --profile box` run by a taw whose own `HOME` is `taw_home`, in a
/// pane whose shell has the server's global `HOME` (`pane_home` when given, else the rig's own).
fn start_with_homes(rig: &Rig, taw_home: &Path, pane_home: Option<&Path>) -> String {
    rig.profile("box", BOX);
    if let Some(home) = pane_home {
        rig.server
            .tmux_ok(&["set-environment", "-g", "HOME", home.to_str().unwrap()]);
    }
    let pane = rig.window();
    let (code, env) = rig.server.taw_with_env(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--sandbox",
            "--profile",
            "box",
            "--timeout",
            "60000",
        ],
        &[("HOME", taw_home.to_str().unwrap())],
    );
    assert_eq!(code, 0, "{env}");
    pane
}

/// The folder above taw's `target/<profile>`: a folder taw's own path lies under.
fn above_taw() -> PathBuf {
    Path::new(common::taw_bin())
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

/// The home written as `~` is the pane's, not that of the taw that types: a taw whose `HOME` is a
/// folder its path lies under, in a pane whose home is another, is typed by its absolute path, and
/// the line runs.
#[test]
fn the_typed_taw_is_not_written_under_the_tilde_of_the_typing_taws_home() {
    let rig = Rig::new();
    let pane = start_with_homes(&rig, &above_taw(), None);

    let typed = typed_taw(&rig, &pane);

    assert_eq!(
        typed,
        taw::sandbox::typed::word(common::taw_bin()),
        "{typed}"
    );
    assert!(
        wait_for(|| !rig.fake.runs().is_empty(), SETTLE),
        "{typed}: {}",
        rig.server.capture(&pane)
    );
    rig.stop();
}

/// And a taw typed with a `~` is under the pane's home: the server's global `HOME` is a folder taw's
/// path lies under, and the typing taw's own is not, and the line runs.
#[test]
fn the_typed_taw_is_written_under_the_tilde_of_the_panes_home() {
    let rig = Rig::new();
    let home = above_taw();
    let pane = start_with_homes(&rig, rig.server.home_dir(), Some(&home));

    let typed = typed_taw(&rig, &pane);

    let rest = Path::new(common::taw_bin()).strip_prefix(&home).unwrap();
    assert_eq!(
        typed,
        format!("~/{}", taw::sandbox::typed::word(&rest.to_string_lossy())),
        "{typed}"
    );
    assert!(
        wait_for(|| !rig.fake.runs().is_empty(), SETTLE),
        "{typed}: {}",
        rig.server.capture(&pane)
    );
    rig.stop();
}

/// Through a real pane's shell: the agent's arguments reach the container as they
/// were given, a space, a quote and a `$(...)` in one, and a newline, a tab and a C-u, which the line
/// editor of the pane's shell would act on if they were typed as themselves, and a `!`, which a shell
/// with history expansion reads as an event.
#[test]
fn the_agents_arguments_reach_the_container_as_they_were_given() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    let args = [
        "--model",
        "it's $(touch pwned) here",
        "two\nlines",
        "a\ttab",
        "kill\u{15}line",
        "~/x",
        "=x",
        // Typed bare, `!x` is a history expansion to the rig's interactive sh ("event not found").
        "!x",
        "a !x b",
    ];
    let mut command = vec![
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "box",
        "--timeout",
        "60000",
        "--",
    ];
    command.extend(args);

    rig.server.ok(&command, "cli:agent:start", "agent");

    assert!(
        wait_for(|| !rig.fake.runs().is_empty(), SETTLE),
        "{:?}",
        rig.fake.calls()
    );
    let run = &rig.fake.runs()[0];
    assert_eq!(run[run.len() - args.len() - 1..][0], "claude", "{run:?}");
    assert_eq!(run[run.len() - args.len()..], args, "{run:?}");
    assert!(
        !std::fs::canonicalize(rig.server.work_dir())
            .unwrap()
            .join("pwned")
            .exists(),
        "a shell ran the agent's argument"
    );
    rig.stop();
}

/// A profile that runs the agent on this machine, outside any container, has started no sandboxed
/// agent: a process named for the agent in front of the pane is what an ordinary start takes for
/// it, and a sandboxed start never does. Here the profile becomes a program called claude.
#[test]
fn a_profile_that_runs_the_agent_outside_a_container_is_no_sandboxed_start() {
    let rig = Rig::new();
    let fake = common::named_agent(&rig.scratch.join("host"), "claude", None);
    rig.profile(
        "host",
        &format!(
            "#!/bin/sh\nexec '{}' \"$@\"\n",
            fake.join("claude").display()
        ),
    );
    let pane = rig.window();

    let (code, env) = rig.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "host",
        "--timeout",
        "5000",
    ]);

    assert_eq!(
        rig.server.display(&pane, "#{pane_current_command}"),
        "claude"
    );
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "timeout", "{env}");
    assert_eq!(
        env["error"]["message"],
        format!(
            "taw saw no agent in pane {pane}: a profile must end by running the runtime, with the agent as the container's program"
        )
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_kind"), "");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_name"), "");
    rig.server.wake("taw-fake-claude-never");
}

/// A config taw cannot read whole fails closed here as it does for `taw sandbox` itself: with no
/// --profile it cannot tell which profile you chose, so nothing is typed and it says why; with
/// --profile the flag decides, and the agent starts.
#[test]
fn a_config_taw_cannot_read_whole_types_nothing_unless_a_profile_is_named() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    rig.server.write_config("[sandbox\nprofile = \"box\"\n");
    let pane = rig.window();
    let start = |profile: &[&str]| {
        let mut args = vec![
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--sandbox",
            "--timeout",
            "60000",
        ];
        args.extend(profile);
        rig.server.taw(&args)
    };

    let (code, env) = start(&[]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");
    let message = env["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(" did not load (")
            && message.ends_with(
                ", so taw cannot tell which profile you chose: fix it (taw config check says what is wrong), or name one with --profile"
            ),
        "{env}"
    );
    assert!(
        !rig.server.capture(&pane).contains("sandbox"),
        "{}",
        rig.server.capture(&pane)
    );

    let (code, env) = start(&["--profile", "box"]);

    assert_eq!(code, 0, "{env}");
    assert_eq!(
        env["result"]["agent"]["sandbox"],
        json!({"profile": "box", "runtime": "docker"}),
        "{env}"
    );
    rig.stop();
}

/// A `tmux` for one `agent start` that holds its first call matching `pattern` (a `case` pattern over
/// its arguments) until `go` is in `dir`, saying so by making `held` there first: the moment between the
/// start's look at the pane and what it sends there.
fn tmux_holding(dir: &Path, pattern: &str) -> PathBuf {
    let wrapper = dir.join("tmux-holding");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  {pattern})\n    if mkdir {held} 2>/dev/null; then\n      \
             i=0\n      while [ ! -e {go} ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\n    \
             fi ;;\nesac\nexec {real} \"$@\"\n",
            held = quoted(&dir.join("held").display().to_string()),
            go = quoted(&dir.join("go").display().to_string()),
            real = quoted(
                &taw::tmux::Tmux::with_socket_name("unused")
                    .bin
                    .display()
                    .to_string()
            ),
        ),
    )
    .unwrap();
    crate::common::make_executable(&wrapper);
    wrapper
}

/// What takes a pane from its shell while a sandboxed start's line is on its way to it.
#[derive(Clone, Copy, PartialEq)]
enum Taken {
    /// A program the shell runs, before the line is typed.
    BeforeTheLine,
    /// The same, once the line is typed and before its Enter: the line is cleared with C-c first.
    BeforeTheEnter,
}

/// The line, and its Enter, go to the pane only while the shell the start found at its prompt is still
/// in front of it: a program started there in between (`cat`) is given none of it, and the start says
/// so and starts nothing.
fn a_program_in_front_of_the_shell_gets_nothing_of_the_line(taken: Taken) {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    let hold = rig.scratch.join("hold");
    std::fs::create_dir_all(&hold).unwrap();
    let pattern = match taken {
        Taken::BeforeTheLine => "*send-keys*sandbox*",
        Taken::BeforeTheEnter => "*'send-keys -t '*' Enter '*",
    };
    let wrapper = tmux_holding(&hold, pattern);
    let typed_into_cat = rig.scratch.join("typed-into-cat");

    let (code, env) = std::thread::scope(|scope| {
        let start = scope.spawn(|| {
            rig.server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--sandbox",
                    "--profile",
                    "box",
                    "--timeout",
                    "15000",
                ],
                &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
            )
        });
        let held = wait_for(|| hold.join("held").exists(), SETTLE);
        if held {
            if taken == Taken::BeforeTheEnter {
                assert!(
                    rig.screen_holds(&pane, "sandbox claude --profile box"),
                    "{}",
                    rig.server.capture(&pane)
                );
                rig.server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
            }
            rig.type_line(
                &pane,
                &format!("cat > {}", quoted(&typed_into_cat.display().to_string())),
            );
            assert!(
                rig.wait_for_command(&pane, "cat"),
                "cat never came up: {}",
                rig.server.capture(&pane)
            );
        }
        std::fs::write(hold.join("go"), "").unwrap();
        let outcome = start.join().unwrap();
        assert!(held, "the start never sent the line: {outcome:?}");
        outcome
    });

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "shell_not_ready", "{env}");
    let said = match taken {
        Taken::BeforeTheLine => format!(
            "pane {pane} had another program in front of its shell when the command was to be typed, so nothing was typed. Start the agent again once the shell is at its prompt."
        ),
        Taken::BeforeTheEnter => format!(
            "pane {pane} had another program in front of its shell when the command was to be run, and the command typed into it may be left on its line; clear the line and start the agent again once the shell is at its prompt."
        ),
    };
    assert_eq!(env["error"]["message"], said, "{env}");
    for option in [
        "@agent_kind_hint",
        "@agent_acquiring",
        "@agent_typed_line",
        "@agent_sandbox",
    ] {
        assert_eq!(rig.server.pane_opt(&pane, option), "", "{option}");
    }
    // cat writes each line it is given, and the end of its input ends it.
    rig.server.tmux_ok(&["send-keys", "-t", &pane, "C-d"]);
    assert!(
        wait_for(
            || rig.server.display(&pane, "#{pane_current_command}") != "cat",
            SETTLE
        ),
        "cat never ended"
    );
    assert_eq!(
        std::fs::read_to_string(&typed_into_cat).unwrap_or_default(),
        "",
        "the start typed into cat"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

#[test]
fn a_program_in_front_of_the_shell_before_the_line_is_typed_gets_none_of_it() {
    a_program_in_front_of_the_shell_gets_nothing_of_the_line(Taken::BeforeTheLine);
}

#[test]
fn a_program_in_front_of_the_shell_before_the_enter_is_pressed_gets_no_enter() {
    a_program_in_front_of_the_shell_gets_nothing_of_the_line(Taken::BeforeTheEnter);
}

/// The transcript path a sandboxed Claude reports: a path inside its container, not on the host.
const CONTAINER_TRANSCRIPT: &str = "/home/agent/.claude-alt/projects/x/4b1f2c.jsonl";

/// A session `work` with one pane holding a sandboxed Claude, as the watcher and the report reader
/// leave it, saved, and the session then killed. The restored pane comes from the rig's
/// `default-command`, so its `PATH` is the rig's.
fn saved_sandboxed_session(rig: &Rig, profile: &str) {
    let pane = rig.server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        rig.server.work_dir().to_str().unwrap(),
    ]);
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_session_id", "4b1f2c"),
        ("@agent_session_path", CONTAINER_TRANSCRIPT),
        ("@agent_sandbox", profile),
        ("@agent_container", "docker"),
    ] {
        rig.server.set_pane_opt(&pane, option, value);
    }
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    rig.server.tmux_ok(&["kill-session", "-t", "=work"]);
}

#[test]
fn a_restore_types_taw_sandbox_with_the_agents_resume_arguments() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    saved_sandboxed_session(&rig, "box");

    let restored = rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let pane = rig.server.pane_of("work");
    assert_eq!(restored["resumed"], json!([pane.clone()]), "{restored}");
    assert!(
        wait_for(|| rig.fake.runs().len() == 1, SETTLE),
        "{}",
        rig.server.capture(&pane)
    );
    let run = rig.fake.runs().remove(0);
    assert_eq!(run[run.len() - 3..], ["claude", "--resume", "4b1f2c"]);
    // Joined, so the 80 columns cannot cut the line whatever the length of the checkout's path.
    assert!(
        rig.screen_holds_joined(&pane, "sandbox claude --profile box -- --resume 4b1f2c"),
        "{}",
        rig.server.capture(&pane)
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_kind_hint"), "");
    // The container's transcript path goes into the kept resume alone, while the container runs
    // (the check after the profile has ended would pass whatever the restore wrote, since the watcher
    // takes the agent's record off with the mark): a plain path would make the next save skip the
    // kept kind and session, and hold a path with neither.
    let kept: Value =
        serde_json::from_str(&rig.server.pane_opt(&pane, "@agent_sandbox_resume")).unwrap();
    assert_eq!(kept["session_path"], CONTAINER_TRANSCRIPT, "{kept}");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_path"), "");
    rig.stop();
}

/// A sandboxed pane a restore resumed holds its conversation in the kept resume alone until its agent
/// reports a session (the container here has no hook): it is still running that conversation, so a
/// second restore, which finds `work` open with the saved window in it, adds no second window and so no
/// second sandbox on the same conversation.
#[test]
fn a_second_restore_adds_no_window_for_a_sandbox_pane_that_holds_its_conversation_in_the_kept_resume()
 {
    let rig = Rig::new();
    rig.profile("box", BOX);
    saved_sandboxed_session(&rig, "box");
    let first = rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let pane = rig.server.pane_of("work");
    assert_eq!(first["resumed"], json!([pane.clone()]), "{first}");
    assert!(
        wait_for(|| rig.fake.runs().len() == 1, SETTLE),
        "{}",
        rig.server.capture(&pane)
    );
    // The conversation is in the kept resume, and the pane's own record holds no session.
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), "");
    assert!(
        !rig.server
            .pane_opt(&pane, "@agent_sandbox_resume")
            .is_empty(),
        "the kept resume is what holds the conversation"
    );

    let again = rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(again["added"], json!([]), "{again}");
    assert!(
        again["skipped"]
            .as_array()
            .unwrap()
            .contains(&json!("work")),
        "{again}"
    );
    let windows = rig
        .server
        .tmux_ok(&["list-windows", "-t", "=work", "-F", "#{window_id}"]);
    assert_eq!(windows.lines().count(), 1, "{again}");
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(rig.fake.runs().len(), 1, "a second sandbox was started");
    rig.stop();
}

#[test]
fn a_restore_whose_profile_is_gone_leaves_the_pane_a_shell_and_says_why() {
    let rig = Rig::new();
    let file = rig.profile("box", BOX);
    saved_sandboxed_session(&rig, "box");
    std::fs::remove_file(file).unwrap();

    let restored = rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let pane = rig.server.pane_of("work");
    assert!(
        restored["plan"].as_array().unwrap().iter().any(|line| line
            == &json!(format!(
                "skip resume in pane {pane}: its sandbox profile box is gone"
            ))),
        "{restored}"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !rig.server.capture(&pane).contains("sandbox"),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(
        !rig.server.capture(&pane).contains("claude"),
        "never the agent alone: {}",
        rig.server.capture(&pane)
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn a_restore_while_the_runtime_is_down_leaves_the_pane_at_its_shell_after_the_profiles_wait() {
    let rig = Rig::with_the_runtime_by_name_and_env(&[("TAW_SANDBOX_WAIT_SECONDS", "2")]);
    rig.fake.down(true);
    saved_sandboxed_session(&rig, "default");

    rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let pane = rig.server.pane_of("work");
    assert!(
        rig.screen_holds_joined(
            &pane,
            "taw sandbox default: docker did not answer in 2 seconds, so the agent was not started"
        ),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(rig.fake.runs().is_empty());
    // taw's own profile asks the version of the claude its PATH finds, here the net's.
    assert_eq!(rig.net.take(), ["claude --version"]);
}

/// A restore takes its own profile mark off a pane only while the mark has no process: once the
/// typed `taw sandbox` has marked the pane with its own, the mark and the agent's record belong to
/// that start and to the watcher, which ends them with its process, though the profile ran for less
/// than the restore looked for it.
#[test]
fn a_restore_leaves_the_marks_a_started_taw_sandbox_set_however_soon_it_ended() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    saved_sandboxed_session(&rig, "box");
    // The fake's run ends the moment it has written itself down: the shell is back at its prompt
    // well before the restore's last look for the agent's process.
    rig.stop();

    let restored = rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let pane = rig.server.pane_of("work");
    assert_eq!(rig.fake.runs().len(), 1, "{:?}", rig.fake.calls());
    assert_eq!(restored["resumed"], json!([pane.clone()]), "{restored}");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    assert!(
        !rig.server.pane_opt(&pane, "@agent_sandbox_pid").is_empty(),
        "taw sandbox marked the pane with its own process"
    );
    // What the restore resumed stays beside the mark, in the kept resume alone: a sandboxed resume
    // writes no kind or session of its own on the pane.
    let kept: Value =
        serde_json::from_str(&rig.server.pane_opt(&pane, "@agent_sandbox_resume")).unwrap();
    assert_eq!(kept["kind"], "claude", "{kept}");
    assert_eq!(kept["session_id"], "4b1f2c", "{kept}");
    assert_eq!(kept["session_path"], CONTAINER_TRANSCRIPT, "{kept}");
    for option in [
        "@agent_kind",
        "@agent_session_id",
        "@agent_session_path",
        "@agent_pid",
    ] {
        assert_eq!(
            rig.server.pane_opt(&pane, option),
            "",
            "{option}: {restored}"
        );
    }
}

/// How the mark a restore's typed `taw sandbox` set ends while the restore is still at the pane.
#[derive(Clone, Copy, PartialEq, Debug)]
enum MarkEnds {
    /// The system will not run the profile once the pane is marked: `taw sandbox` takes its mark
    /// off again (`store::forget_sandbox`).
    ExecFails,
    /// The profile ends at once: the watcher takes the mark off (`store::end_sandbox`).
    ProfileEnds,
}

/// A stand-in for tmux, made in `dir`, for the rig and every taw it runs. A call that marks a pane
/// with a process (`@agent_sandbox_pid`) takes the x bit off the profile `dir/shut` names, when there
/// is one, past every look taw makes first. In a taw run with `TAW_TEST_HOLD` (the restore, or the
/// start, alone), the first call after the one that writes the kept resume (`@agent_sandbox_resume`),
/// or with `TAW_TEST_HOLD_AT` the first call whose words hold it, is held, saying so by making
/// `dir/held`, until `dir/go` is there. A call that marks a pane with a process, outside such a run,
/// waits for `dir/looked` first when `dir/mark_after_look` is there: the typed `taw sandbox` marks the
/// pane (and, failing to run the profile, takes the mark off again) only after the restore's own write
/// and after the test has looked at the held pane, whatever the load. Returns the variables a rig starts
/// with for it.
fn a_tmux_that_holds_the_restore_after_its_mark(dir: &Path) -> Vec<(String, String)> {
    let script = dir.join("tmux-holding-the-restore");
    common::write_script(
        &script,
        "case \" $* \" in\n\
         *\" @agent_sandbox_pid \"*)\n  \
           if [ -z \"${TAW_TEST_HOLD:-}\" ] && [ -e \"$TAW_TEST_DIR/mark_after_look\" ]; then\n    \
             i=0\n    \
             while [ ! -e \"$TAW_TEST_DIR/looked\" ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\n  \
           fi\n  \
           if [ -e \"$TAW_TEST_DIR/shut\" ]; then chmod 644 \"$(cat \"$TAW_TEST_DIR/shut\")\"; fi ;;\n\
         esac\n\
         if [ -n \"${TAW_TEST_HOLD:-}\" ]; then\n  \
           if [ -n \"${TAW_TEST_HOLD_AT:-}\" ]; then\n    \
             case \" $* \" in *\"$TAW_TEST_HOLD_AT\"*) : > \"$TAW_TEST_DIR/armed\" ;; esac\n  \
           fi\n  \
           if [ -e \"$TAW_TEST_DIR/armed\" ] && mkdir \"$TAW_TEST_DIR/held\" 2>/dev/null; then\n    \
             i=0\n    \
             while [ ! -e \"$TAW_TEST_DIR/go\" ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\n  \
           fi\n  \
           if [ -z \"${TAW_TEST_HOLD_AT:-}\" ]; then\n    \
             case \"$*\" in *set-option*@agent_sandbox_resume*) : > \"$TAW_TEST_DIR/armed\" ;; esac\n  \
           fi\n\
         fi\n\
         exec \"$TAW_TEST_REAL_TMUX\" \"$@\"\n",
    );
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    [
        ("TAW_TMUX_BIN", script.display().to_string()),
        ("TAW_TEST_REAL_TMUX", real.display().to_string()),
        ("TAW_TEST_DIR", dir.display().to_string()),
    ]
    .map(|(name, value)| (name.to_string(), value))
    .to_vec()
}

/// Whatever the order: the mark the typed `taw sandbox` sets can end before the restore
/// is done with the pane, and what the restore writes after it must not leave the pane holding the
/// agent's session or kind with no profile beside it, which the next save would keep and the next
/// restore would type outside the sandbox. A save after it holds the profile with the session, or no
/// session and no kind at all.
fn a_restore_whose_mark_ends_before_it_is_done_saves_no_session_without_its_profile(
    ends: MarkEnds,
    look_late: Option<Duration>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let env = a_tmux_that_holds_the_restore_after_its_mark(&dir);
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let rig = Rig::with_env(&env);
    let profile = rig.profile("box", BOX);
    saved_sandboxed_session(&rig, "box");
    match ends {
        MarkEnds::ExecFails => {
            std::fs::write(dir.join("shut"), profile.display().to_string()).unwrap();
            // The restore types the line before it writes its mark: without this, the typed line
            // could mark the pane, fail and end its mark first, and the restore's write would then
            // leave a mark with no process, which this test's wait for an ended mark never sees.
            // The typed line also waits for the test to have looked at the held pane (`looked`):
            // its mark ends on its own, and on a loaded machine that could come before the look.
            std::fs::write(dir.join("mark_after_look"), "").unwrap();
        }
        // The fake's run ends the moment it has written itself down, and the profile with it.
        MarkEnds::ProfileEnds => rig.stop(),
    }

    let (code, restored) = std::thread::scope(|scope| {
        let restore = scope.spawn(|| {
            rig.server
                .taw_with_env(&["session", "restore"], &[("TAW_TEST_HOLD", "1")])
        });
        let held = wait_for(|| dir.join("held").exists(), SETTLE);
        let mut ended = false;
        if held {
            // A test that looks late: what a loaded machine does to the look, forced.
            if let Some(late) = look_late {
                std::thread::sleep(late);
            }
            let pane = rig.server.pane_of("work");
            // The restore has written the kept resume (the hold is the call after it) and holds there:
            // the container's transcript path is in it, and the pane holds no plain one.
            assert!(
                rig.server
                    .pane_opt(&pane, "@agent_sandbox_resume")
                    .contains(CONTAINER_TRANSCRIPT),
                "{}",
                rig.server.pane_opt(&pane, "@agent_sandbox_resume")
            );
            assert_eq!(rig.server.pane_opt(&pane, "@agent_session_path"), "");
            std::fs::write(dir.join("looked"), "").unwrap();
            if ends == MarkEnds::ProfileEnds {
                rig.server
                    .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
            }
            let started = || match ends {
                MarkEnds::ExecFails => rig
                    .server
                    .tmux_ok(&["capture-pane", "-p", "-J", "-t", &pane])
                    .contains("taw sandbox: could not run the profile"),
                MarkEnds::ProfileEnds => rig.fake.runs().len() == 1,
            };
            ended = wait_for(
                || started() && rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
                SETTLE,
            );
            if ends == MarkEnds::ProfileEnds {
                // Nothing reads the pane after this: what the restore writes is what a save finds.
                rig.server
                    .ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
            }
        }
        std::fs::write(dir.join("go"), "").unwrap();
        let outcome = restore.join().unwrap();
        assert!(held, "the restore never wrote its mark: {outcome:?}");
        assert!(
            ended,
            "the mark of taw sandbox never ended: {}",
            rig.server.watcher_logs()
        );
        outcome
    });

    assert_eq!(code, 0, "{restored}");
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let saved = saved_pane(&rig);
    if saved["agent_sandbox"].is_null() {
        for field in [
            "agent_kind",
            "agent_session_id",
            "agent_session_path",
            "agent_resume",
        ] {
            assert_eq!(saved[field], Value::Null, "{ends:?}, {field}: {saved}");
        }
    } else {
        assert_eq!(saved["agent_sandbox"], "box", "{ends:?}: {saved}");
        assert_eq!(saved["agent_session_id"], "4b1f2c", "{ends:?}: {saved}");
    }
    if ends == MarkEnds::ExecFails {
        assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    }
}

#[test]
fn a_restore_whose_profile_cannot_run_saves_no_session_without_its_profile() {
    a_restore_whose_mark_ends_before_it_is_done_saves_no_session_without_its_profile(
        MarkEnds::ExecFails,
        None,
    );
}

/// The same, with the test looking at the held pane two seconds after the hold: the typed `taw sandbox`
/// does not take its mark off before the look, so the look finds what the restore wrote.
#[test]
fn a_restore_whose_profile_cannot_run_saves_no_session_without_its_profile_seen_late() {
    a_restore_whose_mark_ends_before_it_is_done_saves_no_session_without_its_profile(
        MarkEnds::ExecFails,
        Some(Duration::from_secs(2)),
    );
}

#[test]
fn a_restore_whose_profile_ends_at_once_saves_no_session_without_its_profile() {
    a_restore_whose_mark_ends_before_it_is_done_saves_no_session_without_its_profile(
        MarkEnds::ProfileEnds,
        None,
    );
}

/// The same for `taw agent start --sandbox`, which writes the session its codex is resumed with
/// (`-- resume <id>`) once it has seen the agent in its container: the container can end, and the
/// watcher take the mark off, before that lands, and the session must not land without the mark.
#[test]
fn a_sandboxed_start_whose_mark_ends_before_it_is_done_leaves_no_session_without_its_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let env = a_tmux_that_holds_the_restore_after_its_mark(&dir);
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let rig = Rig::with_env(&env);
    rig.profile("box", BOX);
    let pane = rig.server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        rig.server.work_dir().to_str().unwrap(),
    ]);
    assert!(
        wait_for(|| common::at_a_prompt(&rig.server.capture(&pane)), SETTLE),
        "{}",
        rig.server.capture(&pane)
    );

    let (code, started) = std::thread::scope(|scope| {
        let start = scope.spawn(|| {
            rig.server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "codex",
                    "--pane",
                    &pane,
                    "--sandbox",
                    "--profile",
                    "box",
                    "--timeout",
                    "15000",
                    "--",
                    "resume",
                    "x-1",
                ],
                &[
                    ("TAW_TEST_HOLD", "1"),
                    // The record of the kind, the first write once the agent is seen.
                    ("TAW_TEST_HOLD_AT", " @agent_kind codex "),
                ],
            )
        });
        let held = wait_for(|| dir.join("held").exists(), SETTLE);
        let mut ended = false;
        if held {
            rig.server
                .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
            rig.stop();
            ended = wait_for(
                || rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
                SETTLE,
            );
            // Nothing reads the pane after this: what the start writes is what a save finds.
            rig.server
                .ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
        }
        std::fs::write(dir.join("go"), "").unwrap();
        let outcome = start.join().unwrap();
        assert!(held, "the start never recorded its agent: {outcome:?}");
        assert!(
            ended,
            "the mark of taw sandbox never ended: {}",
            rig.server.watcher_logs()
        );
        outcome
    });

    let run = rig.fake.runs().remove(0);
    assert_eq!(
        run[run.len() - 3..],
        ["codex", "resume", "x-1"],
        "{started}"
    );
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let saved = saved_pane(&rig);
    assert_eq!(
        saved["agent_sandbox"],
        Value::Null,
        "{code}, {started}: {saved}"
    );
    // A kind alone is never resumed: a restore types nothing for a pane with no session.
    for field in ["agent_session_id", "agent_session_path", "agent_resume"] {
        assert_eq!(saved[field], Value::Null, "{field}: {saved}");
    }
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox_resume"), "");
}

/// While its container runs, the session a sandboxed codex was started with is kept beside the mark,
/// never plain, and a save holds it with the profile.
#[test]
fn a_sandboxed_start_keeps_the_session_it_resumes_beside_the_mark() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        rig.server.work_dir().to_str().unwrap(),
    ]);
    assert!(
        wait_for(|| common::at_a_prompt(&rig.server.capture(&pane)), SETTLE),
        "{}",
        rig.server.capture(&pane)
    );

    let (code, started) = rig.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "codex",
        "--pane",
        &pane,
        "--sandbox",
        "--profile",
        "box",
        "--timeout",
        "15000",
        "--",
        "resume",
        "x-1",
    ]);

    assert_eq!(code, 0, "{started}");
    let kept: Value =
        serde_json::from_str(&rig.server.pane_opt(&pane, "@agent_sandbox_resume")).unwrap();
    assert_eq!(kept["kind"], "codex", "{kept}");
    assert_eq!(kept["session_id"], "x-1", "{kept}");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), "");
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let saved = saved_pane(&rig);
    assert_eq!(saved["agent_sandbox"], "box", "{saved}");
    assert_eq!(saved["agent_session_id"], "x-1", "{saved}");
    assert_eq!(saved["agent_kind"], "codex", "{saved}");
    rig.stop();
}

/// A pane kept for a gone folder, sandboxed: nothing is typed while its folder is gone, and the
/// command the plan gives for when it is back is the sandbox line, never the agent alone; a later
/// save keeps the folder, the session and the profile, though a watcher reads the pane and you use
/// its shell in between; and once the folder is back, a restore comes back through the profile.
#[test]
fn a_sandboxed_pane_whose_folder_is_gone_keeps_its_profile_until_the_folder_is_back() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let project = rig.scratch.join("api");
    std::fs::create_dir_all(&project).unwrap();
    let pane = rig.server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-P",
        "-F",
        "#{pane_id}",
        "-c",
        project.to_str().unwrap(),
    ]);
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_session_id", "4b1f2c"),
        ("@agent_sandbox", "box"),
        ("@agent_container", "docker"),
    ] {
        rig.server.set_pane_opt(&pane, option, value);
    }
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    rig.server.tmux_ok(&["kill-session", "-t", "=work"]);
    std::fs::remove_dir_all(&project).unwrap();

    let restored = rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let back = rig.server.pane_of("work");
    let line = restored["plan"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|line| line.contains(&format!("saved directory {} is gone", project.display())))
        .unwrap_or_else(|| panic!("the line for a gone folder: {restored}"))
        .to_string();
    assert!(
        line.contains("sandbox claude --profile box -- --resume 4b1f2c"),
        "{line}"
    );
    assert!(
        !line.contains(" claude --resume"),
        "never the agent alone: {line}"
    );
    assert_eq!(restored["not_resumed"], json!([back.clone()]), "{restored}");
    assert_eq!(
        rig.server.pane_opt(&back, "@agent_sandbox"),
        "",
        "nothing runs in it, and nothing says so"
    );

    // A watcher on the pane, and a command of yours in its shell: what the watcher may take off,
    // it has had its chance to.
    rig.server
        .ok(&["watch", "attach", &back], "cli:watch:attach", "ok");
    // The watcher's first read, at once on attach, done before the count is taken; then the `ls`
    // read, with nothing of the pane's output left to read, and what that read decided written.
    assert!(
        wait_for(|| rig.server.watcher_reads(&back) >= 1, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    let seen = reads_after_output(&rig, &back);
    rig.type_line(&back, "ls");
    assert!(
        read_all_output(&rig, &back, seen),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        rig.fake.calls().is_empty(),
        "nothing is started while the folder is gone"
    );

    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let saved: Value = serde_json::from_slice(
        &std::fs::read(rig.server.state_dir().join("sessions/default.json")).unwrap(),
    )
    .unwrap();
    let work = saved["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "work")
        .unwrap();
    let kept = &work["windows"][0]["panes"][0];
    assert_eq!(kept["cwd"], project.to_str().unwrap(), "{kept}");
    assert_eq!(kept["agent_session_id"], "4b1f2c", "{kept}");
    assert_eq!(
        kept["agent_sandbox"], "box",
        "the profile is kept with the session: {kept}"
    );

    std::fs::create_dir_all(&project).unwrap();
    rig.server.tmux_ok(&["kill-session", "-t", "=work"]);
    rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    assert!(
        wait_for(|| rig.fake.runs().len() == 1, SETTLE),
        "{:?}",
        rig.fake.calls()
    );
    let run = rig.fake.runs().remove(0);
    assert_eq!(run[run.len() - 3..], ["claude", "--resume", "4b1f2c"]);
    rig.stop();
}

/// The pane `work` of the last save, as it was saved.
fn saved_pane(rig: &Rig) -> Value {
    let saved: Value = serde_json::from_slice(
        &std::fs::read(rig.server.state_dir().join("sessions/default.json")).unwrap(),
    )
    .unwrap();
    let work = saved["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "work")
        .unwrap()
        .clone();
    work["windows"][0]["panes"][0].clone()
}

/// A restore whose runtime takes its time: every save while its profile waits holds the
/// conversation it is resuming, and once the agent has run and ended, the pane is a shell like any
/// other.
#[test]
fn a_restore_waiting_for_its_runtime_keeps_the_session_it_resumes_in_every_save() {
    let rig = Rig::with_the_runtime_by_name_and_env(&[("TAW_SANDBOX_WAIT_SECONDS", "120")]);
    rig.fake.down(true);
    saved_sandboxed_session(&rig, "default");

    rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let pane = rig.server.pane_of("work");
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        rig.screen_holds_joined(&pane, "taw sandbox default: waiting for docker to answer"),
        "{}",
        rig.server.capture(&pane)
    );
    // A sandboxed resume writes no hint, kind or session of its own on the pane, and the watcher,
    // seeing the profile in front of it (a shell script), records none: the kept resume beside the
    // mark is all a save finds.
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );

    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let kept = saved_pane(&rig);
    assert_eq!(kept["agent_kind"], "claude", "{kept}");
    assert_eq!(kept["agent_session_id"], "4b1f2c", "{kept}");
    assert_eq!(kept["agent_sandbox"], "default", "{kept}");

    // Docker answers: the agent runs, and ends; the container leaves the pane, and the mark with
    // what it kept (here no report comes from inside, so the record never holds the session).
    rig.fake.down(false);
    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    rig.stop();
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_sandbox").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_sandbox_resume"),
        "",
        "it goes with the mark"
    );
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(
        saved_pane(&rig)["agent_session_id"],
        Value::Null,
        "the agent ended: {}",
        saved_pane(&rig)
    );
    // taw's own profile asks the version of the claude its PATH finds, here the net's.
    assert_eq!(rig.net.take(), ["claude --version"]);
}

/// What the sandbox report reader writes while the container runs, set by hand: the
/// session a Claude reported from inside, where it is kept, what started it, and the agent's word.
const READER_WRITES: [(&str, &str); 4] = [
    ("@agent_session_id", "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"),
    (
        "@agent_session_path",
        "/home/agent/.claude/projects/-work/4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d.jsonl",
    ),
    ("@agent_token_session_start_source", "startup"),
    ("@agent_reported_kind", "claude"),
];

/// The sessions a save of the server just made keeps, one for each pane that has one.
fn saved_sessions(rig: &Rig) -> Vec<Value> {
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let saved: Value = serde_json::from_slice(
        &std::fs::read(rig.server.state_dir().join("sessions/default.json")).unwrap(),
    )
    .unwrap();
    saved["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_array().unwrap())
        .filter(|pane| !pane["agent_session_id"].is_null())
        .cloned()
        .collect()
}

/// At the mark's end: from the moment the container is stopped, polls `pane` every 20 ms until
/// its profile mark has gone and then until `done`, and asserts at every poll that a session on the
/// pane has the mark beside it, both read in one tmux command. A save made right after the poll that
/// finds the mark gone keeps no session.
fn the_session_goes_no_later_than_the_mark(rig: &Rig, pane: &str, done: impl Fn() -> bool) {
    let now = || {
        let read = rig.server.display(
            pane,
            "m:#{@agent_sandbox}\ts:#{@agent_session_id}\tp:#{@agent_session_path}",
        );
        // Each word with a prefix of its own: the client's output comes back trimmed.
        let words: Vec<String> = read
            .split('\t')
            .map(|word| word.get(2..).unwrap_or_default().to_string())
            .collect();
        assert_eq!(words.len(), 3, "{read:?}");
        let mark = words[0].clone();
        assert!(
            !mark.is_empty() || (words[1].is_empty() && words[2].is_empty()),
            "the pane holds a session with no profile mark: {read:?}\n{}",
            rig.server.watcher_logs()
        );
        mark
    };
    let poll_until = |until: &dyn Fn(&str) -> bool| {
        let end = std::time::Instant::now() + SETTLE;
        loop {
            if until(&now()) {
                return true;
            }
            if std::time::Instant::now() > end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    rig.stop();
    assert!(
        poll_until(&|mark| mark.is_empty()),
        "the mark never ended: {}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        saved_sessions(rig),
        Vec::<Value>::new(),
        "a save right after the mark's end"
    );
    assert!(
        poll_until(&|_| done()),
        "the pane never settled after the mark's end: {}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        saved_sessions(rig),
        Vec::<Value>::new(),
        "a save once settled"
    );
}

/// A Claude that ends mid-turn inside its sandbox: the read that sees the container leave writes
/// the exit's `idle` and ends the mark, and the record is cleared only at the next read. The session
/// the reader took from inside goes with the mark, in the same tmux command, never after it.
#[test]
fn a_session_from_inside_goes_with_the_mark_when_the_agent_ends_mid_turn() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    draw(&rig.server, &pane, "\u{273b} Claude Code", CLAUDE_WORKING);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_state") == "working"
                && rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    for (option, value) in READER_WRITES {
        rig.server.set_pane_opt(&pane, option, value);
    }
    let kept = saved_sessions(&rig);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        kept[0]["agent_sandbox"], "box",
        "kept with its profile: {kept:?}"
    );

    the_session_goes_no_later_than_the_mark(&rig, &pane, || {
        rig.server.pane_opt(&pane, "@agent_kind").is_empty()
    });
    let logs = rig.server.watcher_logs();
    assert!(
        logs.contains("rule process_exited"),
        "the record's rule is process_exited: {logs}"
    );
}

/// A program the shell runs after the container (`taw sandbox ...; sleep 60`): the watcher misses
/// the agent and keeps its record through several reads, but the mark ends at the read that sees the
/// container leave, and the session the reader took from inside goes with it.
#[test]
fn a_session_from_inside_goes_with_the_mark_when_a_program_follows_the_container() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    let typed = line(&[common::taw_bin(), "sandbox", "--profile", "box"]);
    rig.type_line(&pane, &format!("{typed}; sleep 60"));
    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude"
                && rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    for (option, value) in READER_WRITES {
        rig.server.set_pane_opt(&pane, option, value);
    }
    let kept = saved_sessions(&rig);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        kept[0]["agent_sandbox"], "box",
        "kept with its profile: {kept:?}"
    );

    let at_the_end = std::cell::Cell::new(None);
    the_session_goes_no_later_than_the_mark(&rig, &pane, || {
        let reads = rig.server.watcher_reads(&pane);
        let first = at_the_end.get().unwrap_or(reads);
        at_the_end.set(Some(first));
        reads >= first + 2
    });
    assert_eq!(
        rig.server.display(&pane, "#{pane_current_command}"),
        "sleep"
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_kind"),
        "claude",
        "the record is kept past the mark's end"
    );
}

/// What the watcher of `pane` has logged so far.
fn watch_log(rig: &Rig, pane: &str) -> String {
    std::fs::read_to_string(
        rig.server
            .state_dir()
            .join("logs")
            .join(format!("watch-{pane}.log")),
    )
    .unwrap_or_default()
}

const TOOK: &str = "took the session its claude reported from inside its sandbox";

/// How many times the watcher of `pane` has said it took the session from inside its sandbox.
fn takes(rig: &Rig, pane: &str) -> usize {
    watch_log(rig, pane).matches(TOOK).count()
}

/// The watcher writes the session in tmux first and logs the take after, so a test that has seen the
/// session on the pane waits for the take in the log rather than reading it at once.
fn wait_for_the_first_take(rig: &Rig, pane: &str) {
    assert!(
        wait_for(|| takes(rig, pane) == 1, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
}

#[test]
fn the_session_an_agent_reports_from_inside_is_the_panes_and_never_reaches_a_log() {
    let rig = Rig::with_env(&[("FAKE_RUNTIME_REPORT", CLAUDE_START)]);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &["--profile", "box"]);

    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id")
                == "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_session_path"),
        "/home/agent/.claude/projects/-work/4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d.jsonl"
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_reported_kind"), "claude");
    assert_eq!(
        rig.server
            .pane_opt(&pane, "@agent_token_session_start_source"),
        "startup"
    );
    let logs = rig.server.watcher_logs();
    assert!(
        logs.contains("took the session its claude reported from inside its sandbox"),
        "{logs}"
    );
    assert!(
        !logs.contains("4b1f2c3d"),
        "nothing the file holds reaches a log: {logs}"
    );
    rig.stop();
}

#[test]
fn a_report_that_is_a_link_a_fifo_or_too_big_is_not_taken_and_the_screen_is_still_read() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude"
                && rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let file = rig.report(&pane).join("session.json");
    let yours = rig.scratch.join("yours.json");
    std::fs::write(&yours, CLAUDE_START).unwrap();
    let state = || rig.server.pane_opt(&pane, "@agent_state");
    // The watcher's own word for each refusal: a positive wait, so that the file was looked at
    // while it was there.
    let refused = |why: &str| {
        let line = format!("pane {pane}: its sandbox's report was not read: {why}");
        wait_for(|| rig.server.watcher_logs().contains(&line), SETTLE)
    };

    std::os::unix::fs::symlink(&yours, &file).unwrap();
    draw(&rig.server, &pane, "\u{273b} Claude Code", CLAUDE_WORKING);
    assert!(
        wait_for(|| state() == "working", SETTLE),
        "the screen is still read: {}",
        rig.server.watcher_logs()
    );
    assert!(
        refused("it is a symbolic link"),
        "{}",
        rig.server.watcher_logs()
    );
    std::fs::remove_file(&file).unwrap();

    assert!(
        std::process::Command::new("mkfifo")
            .arg(&file)
            .status()
            .unwrap()
            .success()
    );
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(|| state() != "working", SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        refused("it is not a regular file"),
        "{}",
        rig.server.watcher_logs()
    );
    std::fs::remove_file(&file).unwrap();

    let padded = format!("{CLAUDE_START}{}", " ".repeat(65_536));
    std::fs::write(&file, padded).unwrap();
    draw(&rig.server, &pane, "\u{273b} Claude Code", CLAUDE_WORKING);
    assert!(
        wait_for(|| state() == "working", SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        refused("it is over 64 KiB"),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        rig.server.pane_opt(&pane, "@agent_session_id"),
        "",
        "none of the three was taken"
    );

    // The same report as a regular file is taken: the three above were refused for what they were.
    std::fs::remove_file(&file).unwrap();
    rig.report_from_inside(&pane, CLAUDE_START);
    assert!(
        wait_for(
            || !rig.server.pane_opt(&pane, "@agent_session_id").is_empty(),
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    rig.stop();
}

/// A new pane of the session `work`, the session made with it when `first`, once its shell is at
/// its prompt: a session of its own, so that killing it leaves the server running for the restore.
fn work_pane(rig: &Rig, first: bool) -> String {
    let dir = rig.server.work_dir().to_str().unwrap().to_string();
    let pane = match first {
        true => rig.server.tmux_ok(&[
            "new-session",
            "-d",
            "-s",
            "work",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            &dir,
        ]),
        false => rig.server.tmux_ok(&[
            "new-window",
            "-t",
            "=work:",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            &dir,
        ]),
    };
    assert!(
        wait_for(|| common::at_a_prompt(&rig.server.capture(&pane)), SETTLE),
        "the shell of {pane} never came to its prompt: {:?}",
        rig.server.capture(&pane)
    );
    pane
}

/// Two sandboxed agents started at once keep their own report folders, container
/// names and sessions, and a restore brings each back in its own pane.
#[test]
fn two_sandboxed_agents_started_at_once_keep_their_own_folders_names_and_sessions() {
    const LEFT: &str = "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const RIGHT: &str = "22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let rig = Rig::with_the_runtime_by_name();
    let (left, right) = (work_pane(&rig, true), work_pane(&rig, false));
    for pane in [&left, &right] {
        rig.server
            .ok(&["watch", "attach", pane], "cli:watch:attach", "ok");
    }

    rig.sandbox(&left, &[]);
    rig.sandbox(&right, &[]);

    assert!(
        rig.wait_for_command(&left, "docker") && rig.wait_for_command(&right, "docker"),
        "{}",
        rig.server.capture(&left)
    );
    assert!(
        wait_for(|| rig.fake.runs().len() == 2, SETTLE),
        "{:?}",
        rig.fake.runs()
    );
    let runs = rig.fake.runs();
    let after = |run: &[String], flag: &str| {
        run[run.iter().position(|word| word == flag).unwrap() + 1].clone()
    };
    assert_ne!(after(&runs[0], "--name"), after(&runs[1], "--name"));
    let volume = |pane: &str| format!("{}:/run/taw", rig.report(pane).display());
    let mut mounted: Vec<String> = runs
        .iter()
        .filter_map(|run| run.iter().find(|word| word.ends_with(":/run/taw")).cloned())
        .collect();
    mounted.sort();
    let mut expected = vec![volume(&left), volume(&right)];
    expected.sort();
    assert_eq!(mounted, expected);

    for (pane, id) in [(&left, LEFT), (&right, RIGHT)] {
        draw(&rig.server, pane, "\u{2733} Claude Code", CLAUDE_IDLE);
        rig.report_from_inside(
            pane,
            &CLAUDE_START.replace("4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d", id),
        );
    }
    assert!(
        wait_for(
            || rig.server.pane_opt(&left, "@agent_session_id") == LEFT
                && rig.server.pane_opt(&right, "@agent_session_id") == RIGHT,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );

    // A restart: the layout saved, its session gone, and restored.
    let windows = [
        (rig.server.display(&left, "#{window_index}"), LEFT),
        (rig.server.display(&right, "#{window_index}"), RIGHT),
    ];
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    rig.server.tmux_ok(&["kill-session", "-t", "=work"]);
    rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    assert!(
        wait_for(|| rig.fake.runs().len() == 4, SETTLE),
        "{:?}",
        rig.fake.runs()
    );
    for (window, id) in windows {
        let pane = rig.server.display(&format!("=work:{window}"), "#{pane_id}");
        let run = rig
            .fake
            .runs()
            .into_iter()
            .skip(2)
            .find(|run| run.contains(&volume(&pane)))
            .unwrap_or_else(|| panic!("no run in the restored pane {pane}"));
        assert_eq!(run[run.len() - 2..], ["--resume", id], "{run:?}");
    }
    rig.stop();
    // taw's own profile asks the version of the claude its PATH finds, the net's, at each start.
    let taken = rig.net.take();
    assert!(
        !taken.is_empty() && taken.iter().all(|call| call == "claude --version"),
        "{taken:?}"
    );
}

/// A container that comes again into a pane whose look at the runtime
/// was given up on drops what the agent reported of itself as it comes (`store::mark_container`),
/// the session the report gave included. The watcher takes the same report again, though the file
/// has not changed.
#[test]
fn a_session_from_inside_is_taken_again_after_a_container_comes_back_and_drops_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (rig, ps, pane, (_started, stopped, runs)) = sandbox_with_a_ps_that_can_refuse(tmp.path());
    rig.report_from_inside(&pane, CLAUDE_START);
    let id = "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id") == id,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    wait_for_the_first_take(&rig, &pane);

    // The look is given up on: the runtime goes from the pane, and the mark stays.
    ps.refuse();
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.pane_opt(&pane, "@agent_container").is_empty()
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        wait_for(|| stopped.exists(), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");

    // It comes back, and drops what the agent reported as it comes; the report is taken again.
    ps.answer();
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                hook_runs(&runs) == ["sandbox.started", "sandbox.stopped", "sandbox.started"]
            },
            SETTLE
        ),
        "{:?}",
        hook_runs(&runs)
    );
    assert!(
        wait_for(|| takes(&rig, &pane) == 2, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id") == id,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    rig.stop();
}

/// A session taken from inside that someone else takes off the pane (here by hand, as another
/// agent's report would) is taken again from the same report at the next look.
#[test]
fn a_session_from_inside_dropped_by_someone_else_is_taken_again() {
    let rig = Rig::with_env(&[("FAKE_RUNTIME_REPORT", CLAUDE_START)]);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    let id = "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id") == id,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    wait_for_the_first_take(&rig, &pane);

    for option in ["@agent_session_id", "@agent_session_path"] {
        rig.server
            .tmux_ok(&["set-option", "-u", "-p", "-t", &pane, option]);
    }

    assert!(
        wait_for(|| takes(&rig, &pane) == 2, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), id);
    rig.stop();
}

/// A stand-in for tmux, made in `dir`, for a rig's watchers and every taw the rig runs: once, around
/// the first `if-shell` call that writes the session the reader took from inside (it sets
/// `@agent_reported_kind` to claude), it sets `option` of the pane that call is about to `moved`, as
/// a fresh `taw sandbox` marks the pane between the watcher's look and its write, and puts back what
/// the pane held as the call ends; every call runs the real tmux. Returns the variables a rig
/// starts with for it ([`Rig::with_env`]), and the folder whose making says the call was moved around.
fn a_tmux_that_moves_a_mark_around_the_readers_write(
    dir: &Path,
    option: &str,
    moved: &str,
) -> (Vec<(String, String)>, PathBuf) {
    let script = dir.join("tmux-that-moves-a-mark");
    let done = dir.join("mark-moved");
    common::write_script(
        &script,
        "case \"$*\" in\n\
         *if-shell*\"@agent_reported_kind claude\"*)\n  \
           if mkdir \"$TAW_TEST_MOVED\" 2>/dev/null; then\n    \
             pane=\n    \
             prev=\n    \
             for arg in \"$@\"; do\n      \
               if [ \"$prev\" = -t ] && [ -z \"$pane\" ]; then pane=$arg; fi\n      \
               prev=$arg\n    \
             done\n    \
             held=$(\"$TAW_TEST_REAL_TMUX\" \"$1\" \"$2\" show-options -p -q -v -t \"$pane\" \"$TAW_TEST_OPTION\")\n    \
             \"$TAW_TEST_REAL_TMUX\" \"$1\" \"$2\" set-option -p -t \"$pane\" \"$TAW_TEST_OPTION\" \"$TAW_TEST_MOVED_TO\"\n    \
             out=$(\"$TAW_TEST_REAL_TMUX\" \"$@\")\n    \
             status=$?\n    \
             \"$TAW_TEST_REAL_TMUX\" \"$1\" \"$2\" set-option -p -t \"$pane\" \"$TAW_TEST_OPTION\" \"$held\"\n    \
             printf '%s\\n' \"$out\"\n    \
             exit $status\n  \
           fi\n  \
           ;;\n\
         esac\n\
         exec \"$TAW_TEST_REAL_TMUX\" \"$@\"\n",
    );
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let env = [
        ("TAW_TMUX_BIN", script.display().to_string()),
        ("TAW_TEST_REAL_TMUX", real.display().to_string()),
        ("TAW_TEST_OPTION", option.to_string()),
        ("TAW_TEST_MOVED_TO", moved.to_string()),
        ("TAW_TEST_MOVED", done.display().to_string()),
    ]
    .map(|(name, value)| (name.to_string(), value));
    (env.to_vec(), done)
}

/// A report that is read while the pane's mark is what the read saw, and written after `option` of
/// the pane was moved to `moved` (and put back): the write is refused whole, the watcher says the
/// report came as the pane changed, and the next read looks at the same report again and takes it.
fn a_write_refused_by_a_moved_mark_is_taken_at_the_next_read(option: &str, moved: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let (mut env, done) =
        a_tmux_that_moves_a_mark_around_the_readers_write(tmp.path(), option, moved);
    env.push(("FAKE_RUNTIME_REPORT".to_string(), CLAUDE_START.to_string()));
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let rig = Rig::with_env(&env);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));

    // A positive wait on the watcher's own word first: the write was refused, not late.
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.watcher_logs().contains(&format!(
                    "pane {pane}: its sandbox's report came as the pane changed; the next read looks again"
                ))
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(done.exists());
    let log = watch_log(&rig, &pane);
    let refused = log
        .find("its sandbox's report came as the pane changed")
        .expect("the refusal is in the log");
    assert!(
        log.find(TOOK).is_none_or(|took| refused < took),
        "nothing was taken before that write was refused:\n{log}"
    );

    let id = "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.pane_opt(&pane, "@agent_session_id") == id
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    wait_for_the_first_take(&rig, &pane);
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    rig.stop();
}

/// The reader's write is guarded on the mark's process too: a fresh `taw sandbox` of the same profile
/// has another one, and the old container's session lands beside nothing of its own. Refused, and the
/// next read looks again.
#[test]
fn a_session_from_inside_is_not_written_beside_a_fresh_mark_of_the_same_profile() {
    a_write_refused_by_a_moved_mark_is_taken_at_the_next_read("@agent_sandbox_pid", "1");
}

/// A report from the less trusted side whose source is 20,000 characters, in a valid file of 20 KB:
/// refused whole, once, in the watcher's words, with nothing of it in the log; the read does not fail
/// (its screen is still read) and the session is not taken.
#[test]
fn a_report_with_a_20_kb_source_is_refused_once_and_fails_no_read() {
    let rig = Rig::new();
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude"
                && rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let source = "x".repeat(20_000);
    rig.report_from_inside(
        &pane,
        &CLAUDE_START.replace(
            "\"source\":\"startup\"",
            &format!("\"source\":\"{source}\""),
        ),
    );
    let refusal =
        format!("pane {pane}: its sandbox's report was not taken: its source is not one taw keeps");
    assert!(
        wait_for(|| rig.server.watcher_logs().contains(&refusal), SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    // The reads that follow leave the same file alone, and the screen is still read.
    let at_the_refusal = rig.server.watcher_reads(&pane);
    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{273b} Claude Code", CLAUDE_WORKING);
                rig.server.watcher_reads(&pane) >= at_the_refusal + 2
                    && rig.server.pane_opt(&pane, "@agent_state") == "working"
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    let logs = rig.server.watcher_logs();
    assert_eq!(logs.matches(&refusal).count(), 1, "{logs}");
    assert!(!logs.contains("failed ("), "no read failed: {logs}");
    assert!(!logs.contains("xxxxxxxx"), "nothing of the report: {logs}");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), "");
    assert_eq!(takes(&rig, &pane), 0);
    rig.stop();
}

/// Claude's `/clear` (or `/resume`) inside the sandbox writes a new SessionStart, which replaces the
/// session already taken: its id lands, a second take is logged, and a save would resume the new one.
#[test]
fn a_clear_inside_the_sandbox_replaces_the_session_already_taken() {
    let rig = Rig::with_env(&[("FAKE_RUNTIME_REPORT", CLAUDE_START)]);
    rig.profile("box", BOX);
    let pane = rig.window();
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    rig.sandbox(&pane, &["--profile", "box"]);
    assert!(rig.wait_for_command(&pane, "docker"));
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    let first = "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    let second = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id") == first,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    wait_for_the_first_take(&rig, &pane);

    rig.report_from_inside(
        &pane,
        &CLAUDE_START
            .replace(first, second)
            .replace("\"source\":\"startup\"", "\"source\":\"clear\""),
    );

    assert!(
        wait_for(
            || {
                draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
                rig.server.pane_opt(&pane, "@agent_session_id") == second
            },
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert!(
        wait_for(|| takes(&rig, &pane) == 2, SETTLE),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(
        rig.server
            .pane_opt(&pane, "@agent_token_session_start_source"),
        "clear"
    );
    assert!(
        rig.server
            .pane_opt(&pane, "@agent_session_path")
            .contains(second)
    );
    rig.stop();
}

/// A write the pane's mark refuses (here another profile's, for the moment of the write) is looked at
/// again by the next read, and taken: the same file is not skipped as one already looked at.
#[test]
fn a_session_from_inside_whose_write_was_refused_is_taken_at_the_next_read() {
    a_write_refused_by_a_moved_mark_is_taken_at_the_next_read("@agent_sandbox", "other");
}

/// Codex's SessionStart input, as its hook writes it inside a sandbox.
const CODEX_START: &str = r#"{"session_id":"019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b","transcript_path":"/home/agent/.codex/sessions/2026/10/02/rollout-2026-10-02T09-12-01-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl","cwd":"/work","hook_event_name":"SessionStart","model":"gpt-5-codex","source":"startup"}"#;

/// OpenCode's SessionStart, as its TUI plugin writes it inside a sandbox.
const OPENCODE_START: &str = r#"{"hook_event_name":"SessionStart","session_id":"ses_7f3a91c2bffe4Qz1Wm","source":"select","taw_agent":"opencode"}"#;

/// A restart: the server killed, a new one started on the same socket, as tmux comes back after a
/// reboot, and `taw session restore` run in it. The new server reads a config of the test's own, in
/// place of the host's tmux.conf, which gives every pane the rig's `PATH` from its first moment: a
/// server with no `default-command` gives its panes a login shell, which reads `/etc/profile`,
/// where macOS puts the system's folders back in front of a `PATH`.
fn restart(rig: &Rig) {
    rig.server.tmux_ok(&["kill-server"]);
    assert!(
        wait_for(|| rig.server.tmux(&["list-sessions"]).is_err(), SETTLE),
        "the server is gone"
    );
    let conf = rig.scratch.join("restart.conf");
    std::fs::write(
        &conf,
        format!(
            "set -g exit-empty off\n\
             set -g default-shell /bin/sh\n\
             set -g default-command \"export PATH='{0}'; exec /bin/sh\"\n\
             set-environment -g PATH '{0}'\n",
            rig.path
        ),
    )
    .unwrap();
    rig.server.tmux_ok(&[
        "-f",
        conf.to_str().unwrap(),
        "new-session",
        "-d",
        "-s",
        "boot",
        "-c",
        rig.server.work_dir().to_str().unwrap(),
    ]);
    rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
}

/// `agent` started through `box` reports `payload` from inside; the layout is saved, the server
/// killed, and the layout restored into a new one; the fake's second run must end with `resume`.
fn round_trip(agent: &str, payload: &str, id: &str, resume: &[&str]) {
    let rig = Rig::with_env(&[("FAKE_RUNTIME_REPORT", payload)]);
    rig.profile("box", BOX);
    let pane = work_pane(&rig, true);
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    rig.sandbox(&pane, &[agent, "--profile", "box"]);

    assert!(
        rig.wait_for_command(&pane, "docker"),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id") == id,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    restart(&rig);

    assert!(
        wait_for(|| rig.fake.runs().len() == 2, SETTLE),
        "{:?}",
        rig.fake.runs()
    );
    let again = rig.fake.runs().remove(1);
    assert_eq!(again[again.len() - resume.len()..], *resume, "{again:?}");
    let restored = rig.server.pane_of("work");
    let typed = format!("sandbox {agent} --profile box -- {}", resume[1..].join(" "));
    assert!(
        rig.screen_holds_joined(&restored, &typed),
        "{}",
        rig.server.capture(&restored)
    );
    rig.stop();
}

#[test]
fn claude_comes_back_in_its_conversation_inside_its_sandbox() {
    let id = "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    round_trip("claude", CLAUDE_START, id, &["claude", "--resume", id]);
}

#[test]
fn codex_comes_back_in_its_conversation_inside_its_sandbox() {
    let id = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
    round_trip("codex", CODEX_START, id, &["codex", "resume", id]);
}

#[test]
fn opencode_comes_back_in_its_conversation_inside_its_sandbox() {
    let id = "ses_7f3a91c2bffe4Qz1Wm";
    round_trip(
        "opencode",
        OPENCODE_START,
        id,
        &["opencode", "--session", id],
    );
}

/// A restore's resume whose kept resume is gone, because the line
/// it typed started late and the restore took its marks off, has no session on the pane. The agent
/// inside reports one, the report brings it back, and the next save and restore resume that session
/// through the profile, never outside it.
#[test]
fn a_restore_that_lost_its_kept_resume_gets_the_session_back_from_inside_and_resumes_it_in_the_sandbox()
 {
    let rig = Rig::new();
    rig.profile("box", BOX);
    saved_sandboxed_session(&rig, "box");
    rig.server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let pane = rig.server.pane_of("work");
    rig.server
        .ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(|| rig.fake.runs().len() == 1, SETTLE),
        "{}",
        rig.server.capture(&pane)
    );
    assert!(
        wait_for(
            || !rig.server.pane_opt(&pane, "@agent_sandbox_pid").is_empty(),
            SETTLE
        ),
        "taw sandbox marked the pane with its own process"
    );
    // The late line's end: a container the pane's mark has a process for, and no kept resume.
    rig.server.tmux_ok(&[
        "set-option",
        "-u",
        "-p",
        "-t",
        &pane,
        "@agent_sandbox_resume",
    ]);
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox_resume"), "");
    assert_eq!(rig.server.pane_opt(&pane, "@agent_session_id"), "");
    draw(&rig.server, &pane, "\u{2733} Claude Code", CLAUDE_IDLE);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_kind") == "claude"
                && rig.server.pane_opt(&pane, "@agent_container") == "docker",
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );

    let id = "4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    rig.report_from_inside(&pane, CLAUDE_START);
    assert!(
        wait_for(
            || rig.server.pane_opt(&pane, "@agent_session_id") == id,
            SETTLE
        ),
        "{}",
        rig.server.watcher_logs()
    );
    assert_eq!(rig.server.pane_opt(&pane, "@agent_sandbox"), "box");
    rig.server
        .ok(&["session", "save"], "cli:session:save", "session_saved");
    let kept = saved_pane(&rig);
    assert_eq!(kept["agent_session_id"], id, "{kept}");
    assert_eq!(kept["agent_sandbox"], "box", "{kept}");

    restart(&rig);

    assert!(
        wait_for(|| rig.fake.runs().len() == 2, SETTLE),
        "the second restore typed no resume: {:?}",
        rig.fake.runs()
    );
    let again = rig.fake.runs().remove(1);
    assert_eq!(again[again.len() - 3..], ["claude", "--resume", id]);
    let restored = rig.server.pane_of("work");
    assert!(
        rig.screen_holds_joined(
            &restored,
            &format!("sandbox claude --profile box -- --resume {id}")
        ),
        "{}",
        rig.server.capture(&restored)
    );
    assert_eq!(rig.server.pane_opt(&restored, "@agent_sandbox"), "box");
    rig.stop();
}
