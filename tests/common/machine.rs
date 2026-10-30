//! Another machine, for the tests of everything that talks to one: a second throwaway tmux server with a
//! `taw` of its own (the far side), and a stand-in for ssh that runs the far side's `taw` where a real ssh
//! would log in.
//!
//! No test runs a real ssh. A test points `TAW_SSH_BIN` at [`StandIn::bin`], which is a script: it records
//! the arguments it was run with, one file for each call, drops the options up to the destination as ssh
//! does (`-n` gives the far command no input, as ssh's does), picks the far side by the destination (a
//! [`FarSide`]'s name), and runs the remote command line under `sh -c` in a clean environment: the far
//! server's own `taw` environment ([`Server::taw_env`]), a `PATH` ([`far_path`]) that holds the gate's
//! nets first, a `taw` (a link to the binary under test) and the directory of the tmux the tests resolve,
//! and no way to run a real ssh in its turn (the far side's `TAW_SSH_BIN` is the far server's refusing
//! recorder). So `taw --machine studio workspace list` on this side runs `taw workspace list` on
//! the far server's state, and what comes back is the far side's.
//!
//! A destination can be made to fail as ssh does ([`StandIn::fail_as`]), with the words ssh ends on and
//! the status it exits with, so that each way a connection goes wrong can be walked through.

// Each test file that includes this module uses a subset of it; unused-item warnings would
// otherwise fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Server, ServerOptions, taw_bin, write_script};

/// How a destination fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// ssh cannot connect: `Connection refused`, status 255.
    Refused,
    /// ssh gives up waiting: `Operation timed out`, status 255.
    TimedOut,
    /// The key is not accepted, status 255.
    PermissionDenied,
    /// The host key is not the one known, status 255.
    HostKey,
    /// The machine has no `taw`: what its shell says, status 127.
    NoTaw,
    /// The machine has a `taw` of this version, which has no `machine` verb: it answers `--version` with
    /// `taw <version>` and anything else with clap's complaint, status 2.
    OldTaw(String),
    /// It connects and never says a word.
    Hang,
    /// It connects and says something that is not what a feed says.
    Garbage,
    /// It connects, starts a helper that holds its output open the way a ProxyCommand does, and never
    /// says a word. The helper stays up for ten minutes unless something ends it.
    HangWithHelper,
    /// Like `HangWithHelper`, but it ends its helper when it is ended by its own pid (it traps TERM, INT
    /// and HUP), as ssh ends its ProxyCommand. For tests that kill ssh by its pid, as the feed does.
    HangWithProxy,
}

/// A throwaway tmux server on its own, with the folders a `taw` of its own works in: the machine at the
/// other end of a [`StandIn`]. Its name is the destination the stand-in knows it by.
pub struct FarSide {
    pub name: String,
    pub server: Server,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub home: PathBuf,
}

impl FarSide {
    /// A far side called `name` with one session, `garden`.
    pub fn new(name: &str) -> FarSide {
        FarSide::with_session(name, "garden")
    }

    /// A far side called `name` whose first session is `session`.
    pub fn with_session(name: &str, session: &str) -> FarSide {
        let server = Server::start_with(ServerOptions {
            session: session.to_string(),
            ..ServerOptions::default()
        });
        // `automatic-rename` renames a new window from the shell it starts, `zsh` to `bash`, some tenths of a
        // second after it is made, and on a loaded machine seconds after. A feed then sends a state nobody
        // asked for in the middle of a test that has waited for it to go quiet (a rename test read `bash`
        // where it expected its own name). Off, a window keeps the name it was given. (`tests/cli_json.rs`'s
        // fixture does the same for the same reason.)
        server.tmux_ok(&["set-option", "-g", "automatic-rename", "off"]);
        FarSide {
            name: name.to_string(),
            state_dir: server.state_dir().to_path_buf(),
            config_dir: server.config_dir().to_path_buf(),
            home: server.home_dir().to_path_buf(),
            server,
        }
    }
}

/// The stand-in ssh: a script in a folder of its own, which records every call.
pub struct StandIn {
    /// The script to run as ssh (`TAW_SSH_BIN`).
    pub bin: PathBuf,
    /// A folder with one file for each call, in the order of the calls, holding its arguments, each ended
    /// by a NUL.
    pub log: PathBuf,
    dir: tempfile::TempDir,
}

/// `word` as a shell reads it back, in single quotes.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The folders of the gate's nets, as the near process has them: `$TAW_AGENT_NET` and `$TAW_DESKTOP_NET`,
/// each when it is set and not empty. The one place the variables are read.
fn near_nets() -> (Option<String>, Option<String>) {
    let net = |name: &str| std::env::var(name).ok().filter(|dir| !dir.is_empty());
    (net("TAW_AGENT_NET"), net("TAW_DESKTOP_NET"))
}

/// The `PATH` of a far side, for the shell of its `run` script (`taw_dir` is what the script knows the
/// folder of its `taw` as: `$1`): the nets' `bin` folders first, so that what the far `taw` starts by a
/// bare name (git, an agent, a runtime, ssh) finds the gate's refusing stubs and not the real thing, then
/// the folder of the `taw`, the folder of the tmux the tests resolve, `/usr/bin` and `/bin`. A net that is
/// not given, or is empty, adds nothing.
pub fn far_path(
    taw_dir: &str,
    agent_net: Option<&str>,
    desktop_net: Option<&str>,
    tmux_dir: Option<&str>,
) -> String {
    // Inside double quotes in the script: what the shell reads in them is escaped, but for `taw_dir`.
    let literal = |text: &str| {
        let mut out = String::new();
        for c in text.chars() {
            if matches!(c, '"' | '\\' | '$' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let mut dirs: Vec<String> = Vec::new();
    for net in [agent_net, desktop_net].into_iter().flatten() {
        if !net.is_empty() {
            dirs.push(literal(&format!("{net}/bin")));
        }
    }
    dirs.push(taw_dir.to_string());
    if let Some(tmux_dir) = tmux_dir.filter(|dir| !dir.is_empty()) {
        dirs.push(literal(tmux_dir));
    }
    dirs.push("/usr/bin".to_string());
    dirs.push("/bin".to_string());
    dirs.join(":")
}

impl StandIn {
    /// A stand-in that reaches each of `machines` by its name, and no other destination.
    pub fn new(machines: &[&FarSide]) -> StandIn {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let log = root.join("calls");
        for sub in ["calls", "fail", "far", "hangverb"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        // The tmux the tests resolve (`$TAW_TMUX_BIN`, else the one on the PATH), so that a run against
        // another tmux stays on it on the far side too.
        let tmux = taw::tmux::Tmux::with_socket_name("unused").bin;
        let (agent_net, desktop_net) = near_nets();
        for far in machines {
            let place = root.join("far").join(&far.name);
            std::fs::create_dir_all(place.join("bin")).unwrap();
            std::fs::create_dir_all(place.join("oldbin")).unwrap();
            std::os::unix::fs::symlink(taw_bin(), place.join("bin").join("taw")).unwrap();
            let mut env: Vec<(String, String)> = far
                .server
                .taw_env()
                .into_iter()
                .filter(|(name, _)| name != "PATH" && name != "TAW_TMUX_SOCKET_PATH")
                .collect();
            let mut tmux_dir = None;
            if let Some(parent) = tmux.parent().filter(|p| !p.as_os_str().is_empty()) {
                tmux_dir = Some(parent.display().to_string());
                env.push(("TAW_TMUX_BIN".to_string(), tmux.display().to_string()));
            }
            let path = far_path(
                "$1",
                agent_net.as_deref(),
                desktop_net.as_deref(),
                tmux_dir.as_deref(),
            );
            if let Some(tmpdir) = std::env::var_os("TMUX_TMPDIR") {
                env.push((
                    "TMUX_TMPDIR".to_string(),
                    tmpdir.to_string_lossy().into_owned(),
                ));
            }
            let pairs: String = env
                .iter()
                .map(|(name, value)| format!(" {}", quoted(&format!("{name}={value}"))))
                .collect();
            // $1: the folder of the `taw` to find first; $2: the remote command line.
            write_script(
                &place.join("run"),
                // $3 is `tty` when ssh was given `-t`: the terminal's own kind is passed on then, as ssh does with a
                // terminal (a tmux client with none says it cannot clear the screen and ends), and not otherwise.
                &format!(
                    "term=\nif [ \"$3\" = tty ]; then term=${{TERM:+\"TERM=$TERM\"}}; fi\n\
                     exec env -i{pairs} \"PATH={path}\" $term sh -c \"$2\"\n"
                ),
            );
        }
        let script = format!(
            r#"# The stand-in for ssh (tests/common/machine.rs). Records its arguments, drops the options up to the
# destination, and runs the remote command on the far side the destination names.
dir={dir}
n=1
tty=
while ! mkdir "$dir/calls/$n.d" 2>/dev/null; do n=$((n + 1)); done
printf '%s\0' "$@" > "$dir/calls/$n"
# Its own pid, for a test that looks whether whatever ran it ended it.
echo $$ > "$dir/calls/$n.pid"
while [ $# -gt 0 ]; do
  case $1 in
    -o | -p | -l | -i | -F | -J) shift 2 ;;
    # As ssh does: the far command reads nothing from this side.
    -n) exec </dev/null; shift ;;
    -t) tty=tty; shift ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest=${{1:-}}
[ $# -gt 0 ] && shift
unknown() {{
  echo "ssh: Could not resolve hostname $dest: nodename nor servname provided, or not known" >&2
  exit 255
}}
# Stays up until whatever ran this has gone, so that a test that forgets to end it leaves nothing for long. A pid
# that still answers is no proof the parent is there (the pid may be another process by now, or the parent ended and
# nobody has reaped it), so each turn also asks the system who the parent is, and a `sleep` that cannot run ends it
# as well (it would otherwise spin).
until_orphaned() {{
  p=$PPID
  while kill -0 "$p" 2>/dev/null; do
    now=$(echo $(ps -o ppid= -p $$ 2>/dev/null))
    [ -z "$now" ] || [ "$now" = "$p" ] || exit 0
    sleep 1 & wait $! || [ $? -gt 128 ] || exit 0
  done
  exit 0
}}
case $dest in '' | */* | .*) unknown ;; esac
[ -d "$dir/far/$dest" ] || unknown
bindir=$dir/far/$dest/bin
# A verb that is made to hang (see `StandIn::hang_verb`): a call whose far command is that verb says nothing and stays,
# whichever machine it is for. Read on each call, so a test can switch it while the app runs.
for v in "$dir"/hangverb/*; do
  [ -e "$v" ] || continue
  # A file with something in it is the answer (see `StandIn::reply_verb`); an empty one is a hang.
  case "$*" in *" taw '${{v##*/}}' "*) if [ -s "$v" ]; then cat "$v"; exit 0; fi; until_orphaned ;; esac
done
if [ -f "$dir/fail/$dest" ]; then
  read -r how < "$dir/fail/$dest"
  case $how in
    Refused) echo "ssh: connect to host $dest port 22: Connection refused" >&2; exit 255 ;;
    TimedOut) echo "ssh: connect to host $dest port 22: Operation timed out" >&2; exit 255 ;;
    PermissionDenied) echo "$dest: Permission denied (publickey)." >&2; exit 255 ;;
    HostKey) echo "Host key verification failed." >&2; exit 255 ;;
    NoTaw) echo "sh: taw: command not found" >&2; exit 127 ;;
    OldTaw) bindir=$dir/far/$dest/oldbin ;;
    Hang) until_orphaned ;;
    Garbage) echo "this is not json"; until_orphaned ;;
    HangWithHelper) sleep 600 & echo $! > "$dir/calls/$n.helper"; until_orphaned ;;
    # Ends its helper when it is told to end, as ssh ends its ProxyCommand.
    HangWithProxy) sleep 600 & helper=$!; echo $helper > "$dir/calls/$n.helper"
      trap 'kill $helper 2>/dev/null; exit 0' TERM INT HUP; until_orphaned ;;
  esac
fi
# A failing attach (see `StandIn::fail_attach`): a call with a terminal prints the words on standard error, as the
# far tmux client does when it cannot start, and ends with the status the file starts with.
if [ -n "$tty" ] && [ -f "$dir/failattach" ]; then
  read -r status < "$dir/failattach"
  sed 1d "$dir/failattach" >&2
  exit "$status"
fi
# A stubborn attach (see `StandIn::stubborn_attach`): a call with a terminal does not run the far tmux client but
# does what an ssh with a terminal does to it (raw, the far tmux's alternate screen, mouse and a hidden cursor),
# ignores `SIGHUP` and `SIGTERM`, and stays. `exec` keeps the pid, and an ignored signal survives it. The signals
# are ignored FIRST: whatever a test sees of the raw terminal or the alternate screen then implies they are.
if [ -n "$tty" ] && [ -f "$dir/stubborn" ]; then
  trap '' HUP TERM
  stty raw -echo
  printf '\033[?1049h\033[?1000h\033[?25l'
  echo $$ > "$dir/stubborn.pid"
  exec sleep 600
fi
exec "$dir/far/$dest/run" "$bindir" "$*" "$tty"
"#,
            dir = quoted(&root.display().to_string())
        );
        let bin = root.join("ssh");
        write_script(&bin, &script);
        StandIn { bin, log, dir }
    }

    /// The stand-in as a command to run, as `taw` would run ssh: arguments are the caller's, and so are its
    /// standard streams.
    pub fn command(&self) -> Command {
        let script: &Path = &self.bin;
        Command::new(script)
    }

    /// From now on, runs to `destination` fail as `how` says. The next call sees it: no restart.
    pub fn fail_as(&self, destination: &str, how: Failure) {
        let root = self.dir.path().canonicalize().unwrap();
        assert!(
            root.join("far").join(destination).is_dir(),
            "the stand-in has no machine called {destination}"
        );
        let name = match &how {
            Failure::Refused => "Refused",
            Failure::TimedOut => "TimedOut",
            Failure::PermissionDenied => "PermissionDenied",
            Failure::HostKey => "HostKey",
            Failure::NoTaw => "NoTaw",
            Failure::OldTaw(_) => "OldTaw",
            Failure::Hang => "Hang",
            Failure::Garbage => "Garbage",
            Failure::HangWithHelper => "HangWithHelper",
            Failure::HangWithProxy => "HangWithProxy",
        };
        if let Failure::OldTaw(version) = &how {
            write_script(
                &root
                    .join("far")
                    .join(destination)
                    .join("oldbin")
                    .join("taw"),
                &format!(
                    "case $1 in\n  --version) echo {}; exit 0 ;;\nesac\n\
                     echo \"error: unrecognized subcommand 'machine'\" >&2\nexit 2\n",
                    quoted(&format!("taw {version}"))
                ),
            );
        }
        std::fs::write(root.join("fail").join(destination), format!("{name}\n")).unwrap();
    }

    /// From now on (until [`StandIn::answer_verb`]), a call whose far command is `verb` (`pane`, as in `taw pane read`)
    /// connects and never says a word, as [`Failure::Hang`] does, for every machine, while every other verb is
    /// answered as before. The next call sees it: no restart.
    pub fn hang_verb(&self, verb: &str) {
        assert!(
            !verb.is_empty() && !verb.contains(['/', '\'', ' ']),
            "{verb:?} is no verb"
        );
        std::fs::write(
            self.dir
                .path()
                .canonicalize()
                .unwrap()
                .join("hangverb")
                .join(verb),
            "",
        )
        .unwrap();
    }

    /// From now on (until [`StandIn::answer_verb`]), a call whose far command is `verb` prints `stdout` as its answer and
    /// ends with status 0, whichever machine it is for, and the far side is not run: for a test that needs the far
    /// side to say what no real one would (a pane screen holding what a terminal must never be handed).
    pub fn reply_verb(&self, verb: &str, stdout: &str) {
        assert!(!stdout.is_empty(), "an empty answer is a hang");
        std::fs::write(
            self.dir
                .path()
                .canonicalize()
                .unwrap()
                .join("hangverb")
                .join(verb),
            stdout,
        )
        .unwrap();
    }

    /// Ends [`StandIn::hang_verb`] or [`StandIn::reply_verb`] for `verb`: the next call is answered by the far side again.
    pub fn answer_verb(&self, verb: &str) {
        let _ = std::fs::remove_file(
            self.dir
                .path()
                .canonicalize()
                .unwrap()
                .join("hangverb")
                .join(verb),
        );
    }

    /// From now on, a call with a terminal (`-t`: the attach) is an ssh that sets the terminal raw, enters the
    /// alternate screen with the mouse mode and a hidden cursor, ignores `SIGHUP` and `SIGTERM`, and stays until
    /// it is killed. The far tmux client is not run. The next call sees it: no restart.
    pub fn stubborn_attach(&self) {
        std::fs::write(self.dir.path().canonicalize().unwrap().join("stubborn"), "").unwrap();
    }

    /// From now on, a call with a terminal (`-t`: the attach) prints `words` on standard error and ends with
    /// `status`, the way the far tmux client ends when it cannot start (`missing or unsuitable terminal: ...`,
    /// status 1). The far tmux client is not run. The next call sees it: no restart.
    pub fn fail_attach(&self, status: i32, words: &str) {
        std::fs::write(
            self.dir.path().canonicalize().unwrap().join("failattach"),
            format!("{status}\n{words}\n"),
        )
        .unwrap();
    }

    /// Ends [`StandIn::fail_attach`]: the next attach is run on the far side again.
    pub fn attach_again(&self) {
        let _ = std::fs::remove_file(self.dir.path().canonicalize().unwrap().join("failattach"));
    }

    /// The arguments of every call so far, in order of the calls: what the stand-in was run with, before it
    /// dropped any.
    pub fn calls(&self) -> Vec<Vec<String>> {
        calls_in(&self.log)
    }
}

impl StandIn {
    /// The pid of the stand-in of each call so far, in the order of the calls.
    pub fn pids(&self) -> Vec<u32> {
        self.pids_with("pid")
    }

    /// The pid of the stubborn attach (see [`StandIn::stubborn_attach`]), once it has set the terminal and is
    /// about to become `sleep 600`; `None` until its pid file is whole.
    pub fn stubborn_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.path().join("stubborn.pid"))
            .ok()
            .filter(|text| text.ends_with('\n'))?
            .trim()
            .parse()
            .ok()
    }

    /// The pid of the helper each `HangWithHelper` or `HangWithProxy` call started, in the order of the
    /// calls.
    pub fn helper_pids(&self) -> Vec<u32> {
        self.pids_with("helper")
    }

    fn pids_with(&self, extension: &str) -> Vec<u32> {
        let mut found: Vec<(u32, u32)> = std::fs::read_dir(&self.log)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let n = name.strip_suffix(&format!(".{extension}"))?.parse().ok()?;
                        let pid = std::fs::read_to_string(entry.path()).ok()?;
                        Some((n, pid.trim().parse().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found.into_iter().map(|(_, pid)| pid).collect()
    }
}

/// The calls recorded in `log`, in order.
pub fn calls_in(log: &Path) -> Vec<Vec<String>> {
    let mut numbered: Vec<(u32, Vec<String>)> = std::fs::read_dir(log)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_file())
                .filter_map(|entry| {
                    let n = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
                    let bytes = std::fs::read(entry.path()).ok()?;
                    let mut words: Vec<String> = bytes
                        .split(|b| *b == 0)
                        .map(|w| String::from_utf8_lossy(w).into_owned())
                        .collect();
                    // Every word ends in a NUL, which leaves one empty piece after the last.
                    words.pop();
                    Some((n, words))
                })
                .collect()
        })
        .unwrap_or_default();
    numbered.sort_by_key(|(n, _)| *n);
    numbered.into_iter().map(|(_, words)| words).collect()
}
