//! `Tmux` client: runs the `tmux` binary (honouring `$TMUX`/`-L`/`-S` socket overrides),
//! typed queries, one-call pane listing, option get/set, command helpers. tmux is the
//! database — this module only talks to it, it never computes agent state.

pub mod ids;
pub mod version;

use anyhow::Context;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// How to reach a tmux server: by socket name (`-L`) or by socket path (`-S`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socket {
    Name(String),
    Path(PathBuf),
}

/// A tmux client: which `tmux` binary to run, and which server to talk to.
#[derive(Debug, Clone)]
pub struct Tmux {
    pub bin: PathBuf,
    pub socket: Option<Socket>,
}

impl Tmux {
    /// Builds a client from the environment: `$TAW_TMUX_BIN` picks the binary (else
    /// `which tmux`, else the bare name `tmux`, resolved via `$PATH` when run). The
    /// socket is `$TAW_TMUX_SOCKET_NAME` (`-L`), else `$TAW_TMUX_SOCKET_PATH` (`-S`),
    /// else the socket path that is the first comma-separated field of `$TMUX` (`-S`,
    /// the socket of the tmux server this process is itself running inside), else none
    /// (tmux's own default socket).
    pub fn from_env() -> Tmux {
        let bin = resolve_bin();
        let socket = non_empty_env("TAW_TMUX_SOCKET_NAME")
            .map(Socket::Name)
            .or_else(|| {
                non_empty_env("TAW_TMUX_SOCKET_PATH").map(|p| Socket::Path(PathBuf::from(p)))
            })
            .or_else(|| {
                non_empty_env("TMUX").and_then(|v| {
                    v.split(',')
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(|s| Socket::Path(PathBuf::from(s)))
                })
            });
        Tmux { bin, socket }
    }

    /// A client bound to a named socket, for tests: a throwaway `tmux -L <name>` server.
    pub fn with_socket_name(name: &str) -> Tmux {
        Tmux {
            bin: resolve_bin(),
            socket: Some(Socket::Name(name.to_string())),
        }
    }

    /// A `tmux` command aimed at this client's server: the binary, then `-L <name>` or `-S
    /// <path>` when a socket is set, and nothing else. [`Tmux::run`] adds its arguments and
    /// runs it; `taw open` adds the arguments of `attach-session` and replaces its own
    /// process with it, so the terminal it was typed in becomes the client.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        match &self.socket {
            Some(Socket::Name(name)) => {
                cmd.arg("-L").arg(name);
            }
            Some(Socket::Path(path)) => {
                cmd.arg("-S").arg(path);
            }
            None => {}
        }
        cmd
    }

    /// Runs `tmux -u <args>` (with `-L`/`-S` ahead of the `-u` when `socket` is set) and
    /// returns stdout with exactly one trailing newline removed. The environment (including
    /// `LC_ALL`) is inherited untouched — this process neither sets nor clears it. On a
    /// non-zero exit, `Err`'s message includes the attempted args and tmux's stderr text.
    ///
    /// The `-u` makes this a UTF-8 client whatever the locale is: outside tmux (`$TMUX`
    /// unset) a client in `LC_ALL=C`, or with no `LANG`, has the tabs and the non-ASCII of
    /// every value it reads back rewritten as `_`, and every listing here stops parsing.
    /// It is not in [`Tmux::command`], which the attach of a terminal is built from: that
    /// terminal is not ours to declare UTF-8.
    pub fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        self.run_by(args, None)
    }

    /// [`Tmux::run`] that gives up at `deadline`: a tmux that has not ended by then is killed and
    /// reaped (it is this call's own child), and the answer is an error saying so.
    pub fn run_by(&self, args: &[&str], deadline: Option<Instant>) -> anyhow::Result<String> {
        let mut cmd = self.command();
        cmd.arg("-u");
        cmd.args(args);
        let ran = || format!("failed to run `{} {}`", self.bin.display(), args.join(" "));
        let output = match deadline {
            None => cmd.output().with_context(ran)?,
            Some(deadline) => output_by(&mut cmd, deadline)
                .with_context(ran)?
                .ok_or_else(|| {
                    anyhow::Error::new(TimedOut {
                        command: format!("{} {}", self.bin.display(), args.join(" ")),
                    })
                })?,
        };
        if !output.status.success() {
            return Err(CommandFailed {
                command: format!("{} {}", self.bin.display(), args.join(" ")),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }
            .into());
        }
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.ends_with('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    /// Like `run`, discarding stdout — for commands run only for their side effect.
    pub fn run_ok(&self, args: &[&str]) -> anyhow::Result<()> {
        self.run(args).map(|_| ())
    }

    /// `display-message -p -t <target> <format>`.
    pub fn display(&self, target: &str, format: &str) -> anyhow::Result<String> {
        self.run(&["display-message", "-p", "-t", target, format])
    }

    /// When the tmux server started, in unix seconds (`#{start_time}`, which tmux 3.6 prints as
    /// seconds since the epoch, like `taken_unix`), or `None` when there is no server to ask or it
    /// answered with anything but a number. A server with no session answers.
    pub fn server_start_unix(&self) -> Option<u64> {
        self.run(&["display-message", "-p", "#{start_time}"])
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Lists every pane on the server in one `list-panes -a -F` call: the fixed
    /// pane/window/session fields below, plus every `store::OPTION_FORMATS` column,
    /// tab-joined. Every column is wrapped by [`fold_controls`] so tmux flattens any
    /// literal newline, carriage return or tab in a free-text value *before* the reply is
    /// split — see that function for why.
    pub fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>> {
        Ok(self.list_panes_reporting_skipped()?.0)
    }

    /// [`list_panes`](Self::list_panes) plus how many rows of the reply did not parse and
    /// were skipped.
    ///
    /// Callers that only render the listing do not care — a pane missing from the rail for
    /// one refresh is nothing. A caller that *persists* the listing does:
    /// `session::snapshot::take` would otherwise write a snapshot with a pane silently
    /// missing from it and report success, and `session restore` would then rebuild a
    /// layout that never existed. Such a caller reports the count rather than refusing,
    /// because autosave runs unattended and must not be blocked for ever by one bad pane.
    pub fn list_panes_reporting_skipped(&self) -> anyhow::Result<(Vec<PaneInfo>, usize)> {
        let out = self.run(&["list-panes", "-a", "-F", &pane_listing_format()])?;
        Ok(parse_pane_lines(&out, default_pane_title().as_deref()))
    }

    /// [`list_panes`](Self::list_panes), and the output of the tmux command `then` (its words)
    /// after it, in the same invocation: tmux runs the commands of one invocation without any other
    /// client's between them, so what `then` reads is read right after the panes were listed, and for
    /// the cost of one call.
    pub fn list_panes_then(&self, then: &[&str]) -> anyhow::Result<(Vec<PaneInfo>, String)> {
        let format = pane_listing_format();
        let mut args = vec![
            "list-panes",
            "-a",
            "-F",
            &format,
            ";",
            "display-message",
            "-p",
            PANES_END,
            ";",
        ];
        args.extend_from_slice(then);
        let out = self.run(&args)?;
        let (listing, answer) = split_at_panes_end(&out);
        Ok((
            parse_pane_lines(listing, default_pane_title().as_deref()).0,
            answer.to_string(),
        ))
    }

    /// Lists every session. `active_window_id` is derived from `list_windows` — tmux has
    /// no `#{active_window_id}` format variable.
    pub fn list_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        self.list_sessions_by(None)
    }

    /// [`Tmux::list_sessions`] whose tmux calls are all given up at `deadline`.
    pub fn list_sessions_by(&self, deadline: Option<Instant>) -> anyhow::Result<Vec<SessionInfo>> {
        let windows = self.list_windows_by(deadline)?;
        let mut active_window: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for w in &windows {
            if w.window_active {
                active_window.insert(w.session_id.clone(), w.window_id.clone());
            }
        }
        // Folded like `list_panes`: `session_name` is whatever `rename-session` was given
        // and `session_path` is a path, so both can hold a tab or a newline — and this
        // reply is split on lines and then on tabs. See [`fold_controls`].
        let format = [
            "#{session_id}",
            "#{session_name}",
            "#{session_path}",
            "#{session_attached}",
            "#{session_windows}",
            "#{session_last_attached}",
            "#{session_group_size}",
        ]
        .map(fold_controls)
        .join("\t");
        let out = self.run_by(&["list-sessions", "-F", &format], deadline)?;
        out.lines()
            .map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                anyhow::ensure!(
                    f.len() == 7,
                    "list-sessions: expected 7 tab-separated fields, got {}: {:?}",
                    f.len(),
                    line
                );
                let session_id = f[0].to_string();
                let active_window_id = active_window.get(&session_id).cloned().unwrap_or_default();
                Ok(SessionInfo {
                    session_id,
                    session_name: f[1].to_string(),
                    session_path: f[2].to_string(),
                    session_attached: f[3].parse().with_context(|| {
                        format!("list-sessions: invalid session_attached '{}'", f[3])
                    })?,
                    session_windows: f[4].parse().with_context(|| {
                        format!("list-sessions: invalid session_windows '{}'", f[4])
                    })?,
                    // Empty for a session in no group, which is the 0 it is read as; a value that
                    // does not parse is no group either, and costs nothing else.
                    session_group_size: f[6].parse().unwrap_or(0),
                    active_window_id,
                    // Lenient on purpose: an unparsable time must not take the whole
                    // listing down, unlike the two fields above.
                    session_last_attached: f[5].parse().unwrap_or(0),
                })
            })
            .collect()
    }

    /// Lists every window on the server (`list-windows -a`).
    pub fn list_windows(&self) -> anyhow::Result<Vec<WindowInfo>> {
        self.list_windows_by(None)
    }

    /// [`Tmux::list_windows`] given up at `deadline`.
    pub fn list_windows_by(&self, deadline: Option<Instant>) -> anyhow::Result<Vec<WindowInfo>> {
        // `window_name` is free text (`rename-window`, and tmux's own automatic rename
        // from the running command). Folded for the same reason as `list_panes`.
        let format = [
            "#{window_id}",
            "#{session_id}",
            "#{window_index}",
            "#{window_name}",
            "#{window_active}",
            "#{window_panes}",
            "#{window_layout}",
        ]
        .map(fold_controls)
        .join("\t");
        let out = self.run_by(&["list-windows", "-a", "-F", &format], deadline)?;
        out.lines()
            .map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                anyhow::ensure!(
                    f.len() == 7,
                    "list-windows: expected 7 tab-separated fields, got {}: {:?}",
                    f.len(),
                    line
                );
                Ok(WindowInfo {
                    window_id: f[0].to_string(),
                    session_id: f[1].to_string(),
                    window_index: f[2].parse().with_context(|| {
                        format!("list-windows: invalid window_index '{}'", f[2])
                    })?,
                    window_name: f[3].to_string(),
                    window_active: f[4] == "1",
                    window_panes: f[5].parse().with_context(|| {
                        format!("list-windows: invalid window_panes '{}'", f[5])
                    })?,
                    window_layout: f[6].to_string(),
                })
            })
            .collect()
    }

    /// Lists attached clients (empty when none are attached).
    pub fn list_clients(&self) -> anyhow::Result<Vec<ClientInfo>> {
        // `client_name`/`client_tty` are terminal device paths and `session_name` is free
        // text; same fold, same reason as `list_panes`.
        let format = [
            "#{client_name}",
            "#{client_tty}",
            "#{session_id}",
            "#{session_name}",
        ]
        .map(fold_controls)
        .join("\t");
        let out = self.run(&["list-clients", "-F", &format])?;
        out.lines()
            .map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                anyhow::ensure!(
                    f.len() == 4,
                    "list-clients: expected 4 tab-separated fields, got {}: {:?}",
                    f.len(),
                    line
                );
                Ok(ClientInfo {
                    client_name: f[0].to_string(),
                    client_tty: f[1].to_string(),
                    session_id: f[2].to_string(),
                    session_name: f[3].to_string(),
                })
            })
            .collect()
    }

    /// Reads one pane option, `None` when unset. Queries the pane's full option listing
    /// (`show-options -p`) rather than a single named lookup, so this works uniformly
    /// whether `name` is one of ours (`@agent_*`) or a plain tmux option.
    pub fn get_pane_option(&self, pane: &str, name: &str) -> anyhow::Result<Option<String>> {
        let out = self
            .run(&["show-options", "-p", "-t", pane])
            .with_context(|| format!("listing options for pane '{pane}'"))?;
        Ok(parse_option_lines(&out)
            .into_iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v))
    }

    /// `set-option -p -t <pane> <name> <value>`.
    pub fn set_pane_option(&self, pane: &str, name: &str, value: &str) -> anyhow::Result<()> {
        self.run_ok(&["set-option", "-p", "-t", pane, name, value])
    }

    /// `set-option -u -p -t <pane> <name>` (tmux has no separate `unset-option` command).
    pub fn unset_pane_option(&self, pane: &str, name: &str) -> anyhow::Result<()> {
        self.run_ok(&["set-option", "-u", "-p", "-t", pane, name])
    }

    /// Reads one global option, `None` when unset.
    pub fn get_global_option(&self, name: &str) -> anyhow::Result<Option<String>> {
        let out = self
            .run(&["show-options", "-g"])
            .context("listing global options")?;
        Ok(parse_option_lines(&out)
            .into_iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v))
    }

    /// `set-option -g <name> <value>`.
    pub fn set_global_option(&self, name: &str, value: &str) -> anyhow::Result<()> {
        self.run_ok(&["set-option", "-g", name, value])
    }

    /// `set-option -gu <name>` — back to tmux's own default (or unset, for a `@user`
    /// option).
    pub fn unset_global_option(&self, name: &str) -> anyhow::Result<()> {
        self.run_ok(&["set-option", "-gu", name])
    }

    /// One option as set on `session` itself — `None` when the session inherits it, or
    /// tmux does not know it (an unset `@user` option errors). `show-options -t` without
    /// `-A` lists only the session's own values; `-v` prints the raw value, unquoted.
    ///
    /// `name` may be one element of an array option (`status-format[1]`). tmux prints an
    /// element the session's own array lacks as an empty line, which reads as `None` here
    /// just like an element set to the empty string. [`Tmux::run`] has already taken the
    /// one newline `show-options` ends with, so a value that itself ends in a newline keeps
    /// it.
    ///
    /// Raw bytes need a UTF-8 client: tmux rewrites non-ASCII (and control characters) as
    /// `_` otherwise. [`Tmux::run`] is always one (`-u`), so a read is as the option holds it
    /// whatever the locale of this process is.
    pub fn session_option(&self, session: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.run(&["show-options", "-v", "-t", session, name]) {
            Ok(value) => Ok((!value.is_empty()).then_some(value)),
            Err(err) => {
                log::debug!("show-options -t {session} {name}: {err:#}");
                Ok(None)
            }
        }
    }

    /// `set-option -t <session> <name> <value>`. For one element of an array option
    /// (`status-format[0]`) on a session that had no array of its own, tmux creates one
    /// holding only that element: the rest of the global array is *not* copied, so the
    /// session's other elements are then empty rather than inherited.
    pub fn set_session_option(&self, session: &str, name: &str, value: &str) -> anyhow::Result<()> {
        self.run_ok(&["set-option", "-t", session, name, value])
    }

    /// `set-option -u -t <session> <name>`: back to the inherited value. Unsetting one
    /// element (`status-format[0]`) only removes it from the session's own array; the
    /// session inherits the global array again only once the whole option is unset.
    pub fn unset_session_option(&self, session: &str, name: &str) -> anyhow::Result<()> {
        self.run_ok(&["set-option", "-u", "-t", session, name])
    }

    /// `capture-pane -p [-e] -t <pane>`.
    pub fn capture_pane(&self, pane: &str, with_escapes: bool) -> anyhow::Result<String> {
        if with_escapes {
            self.run(&["capture-pane", "-p", "-e", "-t", pane])
        } else {
            self.run(&["capture-pane", "-p", "-t", pane])
        }
    }

    /// The version of `self.bin`, via `tmux -V`.
    pub fn version(&self) -> anyhow::Result<version::TmuxVersion> {
        let out = self.run(&["-V"])?;
        version::parse(&out)
            .ok_or_else(|| anyhow::anyhow!("could not parse tmux version from '{out}'"))
    }
}

/// A tmux command that ran and failed: what [`Tmux::run`] returns as its error. It reads
/// `` `<binary> <args>` failed (<status>): <stderr> ``, as it always has, and keeps how tmux
/// ended and what it said, for a caller that has to tell one end from another: `display-popup
/// -E` ends with the status of the command it ran, says nothing itself, and ends with 129
/// when the popup was closed from outside.
#[derive(Debug, thiserror::Error)]
#[error("`{command}` failed ({status}): {stderr}")]
pub struct CommandFailed {
    /// The binary and its arguments, as the message spells them.
    pub command: String,
    pub status: std::process::ExitStatus,
    /// What tmux wrote to its stderr, trimmed: empty when it said nothing.
    pub stderr: String,
}

/// A bounded tmux call ([`Tmux::run_by`]) that had not ended by its deadline: killed and reaped. It is its
/// own type so a caller can say so in its own words, as the message names the command line.
#[derive(Debug, thiserror::Error)]
#[error("tmux did not answer in time: `{command}`")]
pub struct TimedOut {
    /// The binary and its arguments, as the message spells them.
    pub command: String,
}

impl CommandFailed {
    /// Why it failed, in a line for a person: the first line of what tmux said, or how it
    /// ended when it said nothing. Never the command line.
    pub fn reason(&self) -> String {
        match self.stderr.lines().next().map(str::trim) {
            Some(said) if !said.is_empty() => said.to_string(),
            _ => match self.status.code() {
                Some(code) => format!("tmux ended with status {code}"),
                None => "tmux was stopped by a signal".to_string(),
            },
        }
    }
}

/// The session name tmux will actually use for `name`, with `.` and `:` folded to `_`.
///
/// tmux rewrites both characters in a session name — verified on 3.6b: `new-session -s
/// a.b:c` creates a session named `a_b_c`, and `-s .foo` creates `_foo`. Most likely so a
/// session name can never be read as a target (`session:window.pane`) or as a relative
/// path. It does this **silently**: `new-session` succeeds, under the other name, and says
/// nothing about it.
///
/// So any caller that computes a session name and then expects to find that session again
/// has to apply this first. It is not sufficient on its own — the name tmux answers with
/// (`new-session -P -F '#{session_name}'`) is the only authority, not least because tmux
/// also refuses some names outright — but without it the computed name and the real one
/// disagree for every `.` a repository or branch name contains.
pub fn session_name(name: &str) -> String {
    name.replace(['.', ':'], "_")
}

/// The session called `name`, as a `-t` target: `=name`, which tmux reads as exactly that
/// name. A bare name is matched as a prefix and as a pattern too, and a bare `~`, which is
/// what the home directory's session is called, is the marked pane: `switch-client -t ~`
/// says `no marked target` when nothing is marked, and takes the client to the marked pane's
/// session when something is. tmux expands no format in a target, so a `#` in it is itself, but
/// a word that ends in `;` ends the command as any word does: a name that ends in one is written
/// with a `\` before it ([`crate::store::word`]).
pub fn session_target(name: &str) -> String {
    crate::store::word(&format!("={name}"))
}

/// What `cmd` writes, when it ends by `deadline`: `None` when it did not, and then it has been killed and
/// reaped. Its pipes are read on threads, and waited for no longer than the deadline, so a child that
/// leaves a descendant holding them cannot hold this call.
fn output_by(
    cmd: &mut Command,
    deadline: Instant,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = sender.send(bytes);
        });
        receiver
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
    };
    let Some(status) = status else {
        return Ok(None);
    };
    // One time left for both pipes: stderr waits only what stdout left.
    let read_by = Instant::now()
        + deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(100));
    let Ok(stdout) = stdout.recv_timeout(read_by.saturating_duration_since(Instant::now())) else {
        return Ok(None);
    };
    let Ok(stderr) = stderr.recv_timeout(read_by.saturating_duration_since(Instant::now())) else {
        return Ok(None);
    };
    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

/// Whether `err` is tmux saying there is no server to talk to: `no server running on
/// <socket>` when a server left its socket file behind, `error connecting to <socket> (No
/// such file or directory)` when the socket has never been used. For a caller that would
/// rather start one than fail, such as an open that is about to make the first session.
///
/// Matched on the stderr text [`Tmux::run`] puts in its error chain, and no wider than
/// that: `error connecting to <socket> (Permission denied)` is a server that is there and
/// cannot be reached, which starting another would not fix.
pub fn no_server(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("no server running")
        || (text.contains("error connecting to")
            && (text.contains("(No such file or directory)")
                || text.contains("(Connection refused)")))
}

/// This machine's host name, in full, as `gethostname` gives it and tmux's `#{host}` formats
/// it. `None` when it cannot be read or is not UTF-8.
fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: `buffer` is writable for the `buffer.len()` bytes the call is told, which is
    // the most it writes, and the bytes are only read as far as the first NUL.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec())
        .ok()
        .filter(|host| !host.is_empty())
}

/// The title tmux gives a pane that no program has titled: this machine's host name, all of
/// it, as `#{host}` formats it (`#{host_short}`, the part before the first dot, is not what a
/// pane gets). Checked on tmux 3.6b for the pane of a new session, a split and a new window,
/// and `tests/tmux_client.rs` checks it against the server the tests run on. `None` when the
/// host name cannot be read, as tmux then sets no title either.
///
/// A pane made before the host name changed keeps the old one, which this does not know.
pub fn default_pane_title() -> Option<String> {
    host_name()
}

/// `pane_title` as taw reads it: the title a program set, and none when it is tmux's default,
/// `default_title` ([`default_pane_title`]). That default is no evidence about whatever runs in
/// the pane, and a rule that takes any title for a sign of life, as Codex's `osc_title_idle`
/// takes any title that is not a spinner, would read every pane nobody titled as an agent at
/// rest. herdr's panes start with no title at all, so taw reads what herdr's rules read, and a
/// pane nobody titled has no title wherever taw shows or passes one: the watcher takes it so
/// when it observes a pane (its own display format), and the listing does for everything else
/// ([`PaneInfo::pane_title`]: the popup, `agent explain`, the JSON verbs, hooks and plugins).
pub fn osc_title<'a>(pane_title: &'a str, default_title: Option<&str>) -> &'a str {
    if default_title.is_some_and(|default| default == pane_title) {
        ""
    } else {
        pane_title
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn resolve_bin() -> PathBuf {
    non_empty_env("TAW_TMUX_BIN")
        .map(PathBuf::from)
        .or_else(|| which::which("tmux").ok())
        .unwrap_or_else(|| PathBuf::from("tmux"))
}

/// One tmux pane, plus whatever agent occupies it (`agent` is all-`None`/empty when the
/// pane has no agent).
#[derive(Clone, Debug, serde::Serialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub session_attached: u32,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_active: bool,
    pub window_layout: String,
    pub pane_index: u32,
    pub pane_active: bool,
    /// The pane's own process, or `0` for a dead pane tmux prints no pid for (tmux 3.8 prints
    /// `#{pane_pid}` empty once the program has ended; 3.6b printed the old pid). `0` is no
    /// process, never process 0: every reader of the process table (`crate::watch`) answers
    /// nothing for it.
    pub pane_pid: u32,
    pub pane_current_command: String,
    pub pane_current_path: String,
    /// The title a program set (OSC 2, `select-pane -T`), and empty for a pane nobody titled:
    /// tmux's own default for that is the host name, which [`osc_title`] reads as none.
    pub pane_title: String,
    pub pane_width: u32,
    pub pane_height: u32,
    pub pane_dead: bool,
    /// `#{alternate_on}`: the pane is showing a full-screen program (nvim, lazygit, less,
    /// htop); the preview never wraps such a pane.
    pub alternate_on: bool,
    pub agent: crate::store::AgentRecord,
}

impl PaneInfo {
    /// Whether the pane holds the name its agent record carries, the rule that keeps names
    /// unique among live agents: an agent of a known kind is on record, the pane is not dead
    /// (kept after its program ended, `remain-on-exit`), and its own shell is not back at its
    /// prompt with nothing running in it (`shell_idle`). Without a watcher, the record of an agent
    /// that has exited stays on its pane, name and all, so such a pane holds no live agent,
    /// whatever it records.
    pub fn holds_its_agent_name(&self, shell_idle: impl Fn(&PaneInfo) -> bool) -> bool {
        holds_agent_name(self.agent.kind.is_some(), self.pane_dead, || {
            shell_idle(self)
        })
    }
}

/// The rule for agent names, written once over its parts: a pane holds the name its agent record
/// carries when an agent of a known kind is on record (`kind_known`), the pane is not dead
/// (`pane_dead`), and its own shell is not known idle (`shell_idle`, asked only when the first two
/// leave the answer open, since it reads the process table). [`PaneInfo::holds_its_agent_name`] is
/// this rule for the command line, and the popup's row follows it for its rename check.
pub fn holds_agent_name(
    kind_known: bool,
    pane_dead: bool,
    shell_idle: impl FnOnce() -> bool,
) -> bool {
    kind_known && !(pane_dead || shell_idle())
}

/// One tmux session (a herdr "workspace").
#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub session_attached: u32,
    pub session_windows: u32,
    /// `#{session_group_size}`: how many sessions are in this session's group (`new-session
    /// -t`), itself included, and `0` for one that is in none, where tmux prints nothing. The
    /// sessions of a group share their windows, so closing the last pane of the last window
    /// closes every one of them.
    pub session_group_size: u32,
    pub active_window_id: String,
    /// `#{session_last_attached}`: unix seconds a client last attached to or switched to
    /// this session, `0` when never (tmux prints `0` for a session no client has shown).
    pub session_last_attached: u64,
}

/// One tmux window (a herdr "tab").
#[derive(Clone, Debug, serde::Serialize)]
pub struct WindowInfo {
    pub window_id: String,
    pub session_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_active: bool,
    pub window_panes: u32,
    pub window_layout: String,
}

/// One attached tmux client.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ClientInfo {
    pub client_name: String,
    pub client_tty: String,
    pub session_id: String,
    pub session_name: String,
}

/// `#{...}` formats for `PaneInfo`'s own fields, in the order `list_panes` expects them
/// (before `store::OPTION_FORMATS`, which is appended after).
const PANE_FIXED_FIELDS: &[&str] = &[
    "#{pane_id}",
    "#{session_id}",
    "#{session_name}",
    "#{session_path}",
    "#{session_attached}",
    "#{window_id}",
    "#{window_index}",
    "#{window_name}",
    "#{window_active}",
    "#{window_layout}",
    "#{pane_index}",
    "#{pane_active}",
    "#{pane_pid}",
    "#{pane_current_command}",
    "#{pane_current_path}",
    "#{pane_title}",
    "#{pane_width}",
    "#{pane_height}",
    "#{pane_dead}",
    "#{alternate_on}",
];

/// Wraps a `#{...}` format so tmux replaces every newline, carriage return and tab in the
/// *expanded value* with a space, using its substitution modifier
/// `#{s/<pattern>/<replacement>/:<variable>}` — three of them chained with `;`, which tmux
/// applies left to right to one variable.
///
/// This matters because we ask tmux for many columns joined by tabs and then split the
/// reply into lines and then into columns. Several of those columns are free text a user
/// or an agent can put anything into — `pane_title` (OSC / `select-pane -T`),
/// `session_name`/`window_name` (`rename-session`/`rename-window`), the two paths, and the
/// user-writable `@agent_*` options (`set-option -p @agent_display "a<TAB>b"` round-trips
/// the tab verbatim, and so does a newline).
///
/// Either character would break the *whole* listing, not just its own pane: a tab shifts
/// every later column, and a newline split one pane's row into two half-rows. Both made
/// `parse_pane_line` reject the line and `list_panes` return `Err` — taking out the status
/// rail, the popup, `watch status` and every CLI listing at once, for as long as the option
/// stayed set. Folding server-side keeps one pane on one line with an exact column count no
/// matter what the value holds.
///
/// Each pattern must be the literal character: tmux ends a modifier at the first `:`, so a
/// POSIX class such as `[[:blank:]]` silently expands to nothing here.
pub(crate) fn fold_controls(format: &str) -> String {
    // `#{s/…/…/:<variable>}` names the variable without its own `#{}` wrapper.
    let variable = format
        .strip_prefix("#{")
        .and_then(|f| f.strip_suffix('}'))
        .unwrap_or(format);
    format!("#{{s/\n/ /;s/\r/ /;s/\t/ /:{variable}}}")
}

/// `text` as one word of a tmux command that tmux parses from a string, which is what the commands
/// of `if-shell` are, so that the word comes out as exactly `text`: in double quotes, with what
/// tmux's parser reads for a meaning written as an escape (`cmd-parse.y`, `yylex_token`, the same in
/// 3.2 and 3.6). Inside double quotes `\` starts an escape, `$` names an environment variable and `~`
/// at the start of a quoted piece is a home directory; a line break is kept, but the spaces after it
/// are dropped and a `#` after them starts a comment, so a line break is written as `\n`. Everything
/// else is itself: `;`, `{`, `}`, `#`, `'`, spaces, `%`, and so is every byte of a character that is
/// not ASCII. Nothing in the word is expanded as a format, which a command that is run is not.
pub fn command_word(text: &str) -> String {
    let mut word = String::with_capacity(text.len() + 2);
    word.push('"');
    for c in text.chars() {
        match c {
            '\\' => word.push_str("\\\\"),
            '"' => word.push_str("\\\""),
            '$' => word.push_str("\\$"),
            '~' => word.push_str("\\~"),
            '\n' => word.push_str("\\n"),
            '\r' => word.push_str("\\r"),
            '\t' => word.push_str("\\t"),
            c => word.push(c),
        }
    }
    word.push('"');
    word
}

/// `dir` as the start directory tmux is given for a new session, window, pane or popup (`-c` of
/// `new-session`, `new-window` and `split-window`, `-d` of `display-popup`), so that tmux opens
/// exactly `dir`. tmux expands formats in it: a folder called `C#Sharp` opened in `C<the session's
/// name>harp`, or in the home folder when that was not there, so every `#` is written `##`. And tmux
/// ends a command at a word that ends in `;`, so a folder whose name ends in one is written with a `\`
/// before it, which tmux takes back out ([`crate::store::word`]).
pub fn start_directory(dir: &str) -> String {
    as_tmux_reads_it(dir)
}

/// `name` as the name tmux is given for a session or a window it makes or renames (`-s` and `-n` of
/// `new-session`, `-n` of `new-window`, `rename-session` and `rename-window`), so that it is called
/// exactly `name`. tmux expands formats in each, as in a start directory ([`start_directory`]): a
/// session asked for as `C#Sharp` was called `Charp`, a window `W#Sx` `W<the session's name>x`, and a
/// restore did not find the session it had made and made it again. So every `#` is written `##`, and a
/// name that ends in `;`, which ended tmux's command, is written with a `\` before it. (tmux 3.6b;
/// `break-pane -n` expands nothing, and is given [`crate::store::word`] alone.)
pub fn new_name(name: &str) -> String {
    as_tmux_reads_it(name)
}

/// `text` as one word of a tmux command in which tmux expands formats, written so that tmux reads back
/// exactly `text`: each `#` as `##`, and a trailing `;` with a `\` before it ([`crate::store::word`]).
fn as_tmux_reads_it(text: &str) -> String {
    crate::store::word(&text.replace('#', "##"))
}

/// The line [`Tmux::list_panes_then`] prints between the panes and what follows them.
const PANES_END: &str = "taw: end of the pane listing";

/// What [`Tmux::list_panes_then`] got, split at [`PANES_END`]: the listing before it and the answer
/// after it. The mark is a line of its own that no pane's row can be (a row has a column for each of
/// its fields), whatever a title or a path says; with none, all of it is the listing.
fn split_at_panes_end(out: &str) -> (&str, &str) {
    let mut at = 0;
    for line in out.split_inclusive('\n') {
        if line.trim_end_matches('\n') == PANES_END {
            return (&out[..at], &out[at + line.len()..]);
        }
        at += line.len();
    }
    (out, "")
}

/// The `-F` format of every pane listing: the fixed fields of a pane, and every `@agent_*` option.
fn pane_listing_format() -> String {
    let mut formats: Vec<String> = PANE_FIXED_FIELDS.iter().map(|f| fold_controls(f)).collect();
    formats.extend(
        crate::store::OPTION_FORMATS
            .iter()
            .map(|f| fold_controls(f)),
    );
    formats.join("\t")
}

/// Parses a `list-panes` reply, **skipping** any line that does not parse and logging it,
/// and returning how many were skipped.
///
/// One unreadable pane must cost that pane's record and nothing else. Returning `Err` for
/// the whole reply is what made a single bad `@agent_*` value able to blank the rail, the
/// popup and every CLI listing at once; [`fold_controls`] prevents the known cause, and
/// this keeps any *unknown* one local to the pane it came from. The count is returned
/// rather than only logged so a caller that persists the listing can say what it lost —
/// see [`Tmux::list_panes_reporting_skipped`].
pub(crate) fn parse_pane_lines(out: &str, default_title: Option<&str>) -> (Vec<PaneInfo>, usize) {
    let mut panes = Vec::new();
    let mut skipped = 0usize;
    for line in out.lines() {
        match parse_pane_line(line, default_title) {
            Ok(pane) => panes.push(pane),
            Err(err) => {
                skipped += 1;
                log::warn!("skipping an unreadable pane in the tmux listing: {err:#}");
            }
        }
    }
    (panes, skipped)
}

fn parse_pane_line(line: &str, default_title: Option<&str>) -> anyhow::Result<PaneInfo> {
    // Safe to split the whole tab-joined line on '\t': every column was requested through
    // `fold_controls`, so tmux has already replaced any literal tab inside a value with a
    // space (and any newline, so this really is one whole pane's row).
    let fields: Vec<&str> = line.split('\t').collect();
    let expected = PANE_FIXED_FIELDS.len() + crate::store::OPTION_FORMATS.len();
    anyhow::ensure!(
        fields.len() == expected,
        "list-panes: expected {expected} tab-separated fields, got {}: {:?}",
        fields.len(),
        line
    );
    let field_u32 = |i: usize, name: &str| -> anyhow::Result<u32> {
        fields[i]
            .parse()
            .with_context(|| format!("list-panes: invalid {name} '{}'", fields[i]))
    };
    Ok(PaneInfo {
        pane_id: fields[0].to_string(),
        session_id: fields[1].to_string(),
        session_name: fields[2].to_string(),
        session_path: fields[3].to_string(),
        session_attached: field_u32(4, "session_attached")?,
        window_id: fields[5].to_string(),
        window_index: field_u32(6, "window_index")?,
        window_name: fields[7].to_string(),
        window_active: fields[8] == "1",
        window_layout: fields[9].to_string(),
        pane_index: field_u32(10, "pane_index")?,
        pane_active: fields[11] == "1",
        pane_pid: if fields[12].is_empty() && fields[18] == "1" {
            0
        } else {
            field_u32(12, "pane_pid")?
        },
        pane_current_command: fields[13].to_string(),
        pane_current_path: fields[14].to_string(),
        pane_title: osc_title(fields[15], default_title).to_string(),
        pane_width: field_u32(16, "pane_width")?,
        pane_height: field_u32(17, "pane_height")?,
        pane_dead: fields[18] == "1",
        alternate_on: fields[19] == "1",
        agent: crate::store::parse_record(&fields[PANE_FIXED_FIELDS.len()..]),
    })
}

/// A listing line as `list-panes -a -F` answers it for one pane, for the tests of whatever
/// reads the listing: `title` is `#{pane_title}` and `kind` is `@agent_kind` (empty for no
/// agent). The rest is fixed.
#[cfg(test)]
pub(crate) fn listing_line(pane_id: &str, title: &str, kind: &str) -> String {
    let mut fields = vec![
        pane_id, "$0", "one", "/repo", "1", "@0", "0", "win", "1", "layout", "0", "1", "100",
        "zsh", "/repo", title, "80", "24", "0", "0",
    ];
    assert_eq!(fields.len(), PANE_FIXED_FIELDS.len());
    let mut options = vec![""; crate::store::OPTION_FORMATS.len()];
    options[0] = kind; // @agent_kind
    fields.extend(options);
    fields.join("\t")
}

/// Parses `show-options`' line-oriented output (`name value` per line) into
/// `(name, value)` pairs, unquoting each value the way tmux quotes it in that listing:
/// bare (`hello`), quoted-with-escapes (`"has \"quotes\" in it"`), or `''` for an
/// explicitly empty string.
pub(crate) fn parse_option_lines(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let (name, raw_value) = line.split_once(' ')?;
            Some((name.to_string(), unquote_option_value(raw_value)))
        })
        .collect()
}

fn unquote_option_value(raw: &str) -> String {
    if raw == "''" {
        return String::new();
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        let mut unescaped = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                unescaped.push(chars.next().unwrap_or('\\'));
            } else {
                unescaped.push(c);
            }
        }
        return unescaped;
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A start directory comes out of tmux as the folder it names: tmux expands formats in it, so each `#`
    /// is written `##`, and ends a command at a word that ends in `;`, so that one is escaped.
    #[test]
    fn a_start_directory_is_written_so_that_tmux_reads_back_the_folder() {
        assert_eq!(start_directory("/home/me/api"), "/home/me/api");
        assert_eq!(start_directory("/home/me/C#Sharp"), "/home/me/C##Sharp");
        assert_eq!(
            start_directory("/x/##{session_name}"),
            "/x/####{session_name}"
        );
        assert_eq!(start_directory("/x/semi;"), "/x/semi\\;");
        assert_eq!(start_directory("/x/a;b"), "/x/a;b");
        assert_eq!(start_directory("/x/#;"), "/x/##\\;");
    }

    /// A session or window name comes out of tmux as itself: tmux expands formats in it, so each `#`
    /// is written `##`, and ends a command at a word that ends in `;`, so that one is escaped.
    #[test]
    fn a_new_name_is_written_so_that_tmux_reads_back_the_name() {
        assert_eq!(new_name("api"), "api");
        assert_eq!(new_name("oss/api"), "oss/api");
        assert_eq!(new_name("C#Sharp"), "C##Sharp");
        assert_eq!(new_name("##{session_name}"), "####{session_name}");
        assert_eq!(new_name("semi;"), "semi\\;");
        assert_eq!(new_name("a;b"), "a;b");
        assert_eq!(new_name("#;"), "##\\;");
    }

    #[test]
    fn tmuxs_default_title_is_no_title_to_the_rules() {
        // What tmux 3.6b titles a pane nobody titled: the whole host name.
        assert_eq!(osc_title("studio.local", Some("studio.local")), "");
        assert_eq!(osc_title("studio", Some("studio")), "");
    }

    #[test]
    fn a_title_a_program_set_reaches_the_rules() {
        let host = Some("studio.local");
        for title in [
            "project",
            "\u{2733} Claude Code",
            "\u{280B} project",
            "[ . ] Action Required | project",
            "grok",
            // Not the default, though it holds it or is part of it.
            "studio.local: ~/code",
            "me@studio.local",
            "studio",
            "Studio.Local",
            "studio.local ",
            " studio.local",
        ] {
            assert_eq!(osc_title(title, host), title, "{title:?}");
        }
    }

    #[test]
    fn no_title_stays_none_and_an_unknown_host_names_no_default() {
        assert_eq!(osc_title("", Some("studio.local")), "");
        assert_eq!(osc_title("", None), "");
        // With no host name to compare with, nothing is taken for the default.
        assert_eq!(osc_title("studio.local", None), "studio.local");
        // An empty host name equals only an empty title, which is none already.
        assert_eq!(osc_title("project", Some("")), "project");
    }

    /// The title a listed pane has: none for the one tmux titled itself, as it is for the
    /// popup, the JSON, the hooks and the plugins that read the listing, and what a program
    /// set for another.
    #[test]
    fn a_listed_pane_nobody_titled_has_no_title_and_one_a_program_titled_keeps_it() {
        const HOST: &str = "studio.local";
        let title = |title: &str, default: Option<&str>| {
            parse_pane_line(&listing_line("%1", title, ""), default)
                .unwrap()
                .pane_title
        };
        assert_eq!(title(HOST, Some(HOST)), "", "tmux's own title is no title");
        assert_eq!(title("project", Some(HOST)), "project");
        assert_eq!(
            title(HOST, None),
            HOST,
            "with no host name to compare with, a title is a title"
        );

        let out = format!(
            "{}\n{}",
            listing_line("%1", HOST, "claude"),
            listing_line("%2", "\u{2733} Build", "claude")
        );
        let (panes, skipped) = parse_pane_lines(&out, Some(HOST));
        assert_eq!(skipped, 0);
        assert_eq!(panes[0].pane_title, "");
        assert_eq!(panes[1].pane_title, "\u{2733} Build");
    }

    #[test]
    fn this_machines_host_name_can_be_read() {
        // That it is what tmux gives a new pane is checked against a real server, in
        // `tests/tmux_client.rs`.
        let default = default_pane_title().expect("this machine has a host name");
        assert!(!default.is_empty());
        assert!(!default.contains('\0') && !default.contains('\n'));
    }

    #[test]
    fn unquotes_option_values_the_way_tmux_show_options_prints_them() {
        assert_eq!(unquote_option_value("hello"), "hello");
        assert_eq!(unquote_option_value("''"), "");
        assert_eq!(unquote_option_value("\"bar baz\""), "bar baz");
        assert_eq!(
            unquote_option_value("\"has \\\"quotes\\\" in it\""),
            "has \"quotes\" in it"
        );
    }

    #[test]
    fn parses_name_value_lines_split_on_the_first_space() {
        let out = "@agent_kind claude\n@agent_token_foo \"bar baz\"";
        assert_eq!(
            parse_option_lines(out),
            vec![
                ("@agent_kind".to_string(), "claude".to_string()),
                ("@agent_token_foo".to_string(), "bar baz".to_string()),
            ]
        );
    }

    #[test]
    fn parses_empty_options_listing_as_no_options() {
        assert_eq!(parse_option_lines(""), Vec::<(String, String)>::new());
    }

    /// The command every `tmux` call starts from names the server it is for: `-L` for a
    /// socket name, `-S` for a path, and nothing when the default socket is meant.
    #[test]
    fn a_command_carries_the_socket_it_was_given() {
        let args = |tmux: &Tmux| -> Vec<String> {
            tmux.command()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let bin = PathBuf::from("/usr/bin/tmux");
        let default = Tmux {
            bin: bin.clone(),
            socket: None,
        };
        assert_eq!(default.command().get_program(), bin.as_os_str());
        assert_eq!(args(&default), Vec::<String>::new());
        let named = Tmux {
            bin: bin.clone(),
            socket: Some(Socket::Name("work".to_string())),
        };
        assert_eq!(args(&named), ["-L", "work"]);
        let path = Tmux {
            bin,
            socket: Some(Socket::Path(PathBuf::from("/tmp/tmux-1/sock"))),
        };
        assert_eq!(args(&path), ["-S", "/tmp/tmux-1/sock"]);
    }

    /// Outside tmux, a client that is not in a UTF-8 locale (`LC_ALL=C`, `LANG` unset) has
    /// the tabs and the non-ASCII of every value it reads back rewritten as `_`, and a
    /// listing then does not parse. So every call [`Tmux::run`] makes is a UTF-8 client,
    /// `-u`, after the socket and ahead of its arguments. [`Tmux::command`], which the
    /// attach of a terminal is built from, is not forced: that terminal is not ours to
    /// declare UTF-8.
    #[test]
    fn every_run_is_a_utf8_client_and_the_attach_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let tmux = Tmux {
            bin,
            socket: Some(Socket::Name("s".to_string())),
        };

        tmux.run(&["list-sessions", "-F", "x"]).unwrap();

        let ran: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(ran, ["-L", "s", "-u", "list-sessions", "-F", "x"]);
        assert!(!tmux.command().get_args().any(|arg| arg == "-u"));
    }

    /// A tmux that exits at once but leaves a descendant holding its pipes is given up at the deadline
    /// and one short read later, not at twice that: stderr waits only what stdout left. The stand-in
    /// starts a `sleep` that keeps both pipes open, writes its pid and exits; the test kills that pid.
    #[test]
    fn a_bounded_call_keeps_its_deadline_when_a_descendant_holds_the_pipes() {
        struct Kill(Option<i32>);
        impl Drop for Kill {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    // SAFETY: a pid this test's own stand-in recorded, signalled once.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("holder.pid");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nsleep 20 &\necho $! > '{}'\nexit 0\n",
                pid_file.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let mut cmd = Command::new(&bin);
        let started = Instant::now();
        let answered = output_by(&mut cmd, started + Duration::from_secs(1)).unwrap();
        let took = started.elapsed();
        let holder = Kill(
            std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok()),
        );
        assert!(holder.0.is_some(), "the stand-in never recorded its pid");
        assert!(answered.is_none(), "the pipes were held, so no answer");
        assert!(
            took < Duration::from_millis(1500),
            "the call took {took:?} for a 1 s deadline"
        );
    }

    /// A tmux that ran and said no keeps how it ended and what it said, for a caller that has
    /// to tell one end from another, and reads as it always did.
    #[test]
    fn a_failed_command_keeps_its_status_and_what_tmux_said() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tmux");
        std::fs::write(&bin, "#!/bin/sh\necho 'no such thing' >&2\nexit 3\n").unwrap();
        crate::test_exec::make_executable(&bin);
        let tmux = Tmux {
            bin: bin.clone(),
            socket: None,
        };

        let err = tmux.run(&["kill-session", "-t", "x"]).unwrap_err();

        let failed = err
            .downcast_ref::<CommandFailed>()
            .expect("a typed failure");
        assert_eq!(failed.status.code(), Some(3));
        assert_eq!(failed.stderr, "no such thing");
        assert_eq!(failed.reason(), "no such thing");
        assert_eq!(
            err.to_string(),
            format!(
                "`{} kill-session -t x` failed (exit status: 3): no such thing",
                bin.display()
            )
        );
    }

    /// With nothing on its stderr the reason is how tmux ended, never the command line.
    #[test]
    fn a_failure_that_said_nothing_is_told_by_its_status() {
        use std::os::unix::process::ExitStatusExt;
        let failed = |raw| CommandFailed {
            command: "/usr/bin/tmux display-popup -E x".to_string(),
            status: std::process::ExitStatus::from_raw(raw),
            stderr: String::new(),
        };
        assert_eq!(failed(3 << 8).reason(), "tmux ended with status 3");
        assert_eq!(failed(129 << 8).reason(), "tmux ended with status 129");
        assert_eq!(failed(9).reason(), "tmux was stopped by a signal");
        let said = CommandFailed {
            stderr: "no current client\nand more".to_string(),
            ..failed(1 << 8)
        };
        assert_eq!(said.reason(), "no current client");
    }

    /// What tmux 3.6b prints when it has no server to talk to: a socket nobody has ever used
    /// has no file (`error connecting to`), one a server left behind is `no server running`.
    /// Anything else a command fails with is a different problem.
    #[test]
    fn no_server_is_either_of_the_two_things_tmux_says_about_it() {
        let failed = |stderr: &str| {
            anyhow::anyhow!("`tmux list-windows -a -F x` failed (exit status: 1): {stderr}")
        };
        assert!(no_server(&failed(
            "error connecting to /private/tmp/tmux-501/taw (No such file or directory)"
        )));
        assert!(no_server(&failed(
            "no server running on /private/tmp/tmux-501/taw"
        )));
        assert!(!no_server(&failed("can't find session: api")));
        assert!(!no_server(&failed("duplicate session: api")));
        assert!(!no_server(&failed(
            "error connecting to /tmp/tmux-0/default (Permission denied)"
        )));
        assert!(!no_server(&anyhow::anyhow!("failed to run `tmux`")));
    }

    /// tmux reads a bare name as a prefix or a pattern, and a bare `~` as the marked pane,
    /// which the home directory's session is named. `=` asks for the name and no other.
    #[test]
    fn a_session_target_is_the_name_and_no_other() {
        assert_eq!(session_target("api"), "=api");
        assert_eq!(session_target("~"), "=~");
        assert_eq!(session_target("oss/api"), "=oss/api");
        // tmux expands no format in a target, and a word that ends in `;` ends the command.
        assert_eq!(session_target("C#Sharp"), "=C#Sharp");
        assert_eq!(session_target("semi;"), "=semi\\;");
    }

    #[test]
    fn session_name_folds_the_characters_tmux_rewrites_silently() {
        assert_eq!(session_name("repo--release-1.2"), "repo--release-1_2");
        assert_eq!(session_name("a.b:c"), "a_b_c");
        assert_eq!(session_name(".dotfiles"), "_dotfiles");
        assert_eq!(session_name("plain-name"), "plain-name");
        assert_eq!(session_name(""), "");
    }

    /// What tmux's parser reads for a meaning inside double quotes is written as an escape, and
    /// nothing else is touched. Whether tmux reads the word back as the text is for a real server to
    /// say (`tests/tmux_command_word.rs`).
    #[test]
    fn a_command_word_escapes_what_tmux_reads_inside_double_quotes_and_nothing_else() {
        assert_eq!(command_word("plain text"), "\"plain text\"");
        assert_eq!(command_word(""), "\"\"");
        assert_eq!(command_word(r#"a\b"c"#), r#""a\\b\"c""#);
        assert_eq!(command_word("$HOME ~/x"), r#""\$HOME \~/x""#);
        assert_eq!(
            command_word("one\n  # two\r\tthree"),
            r#""one\n  # two\r\tthree""#
        );
        // Not touched: separators, braces, formats, a percent, quotes of the other kind, other scripts.
        let kept = "a; b {c} #{pane_id} 100% 'q' \u{e9}\u{1f600} x";
        assert_eq!(command_word(kept), format!("\"{kept}\""));
    }

    #[test]
    fn fold_controls_chains_a_substitution_per_line_breaking_character() {
        // `;` separates modifiers and `:` ends the last one — so every pattern has to sit
        // before the colon, and none of them may contain one.
        assert_eq!(
            fold_controls("#{@agent_display}"),
            "#{s/\n/ /;s/\r/ /;s/\t/ /:@agent_display}"
        );
        // A bare variable name (no `#{}` wrapper) is accepted too.
        assert_eq!(
            fold_controls("pane_title"),
            "#{s/\n/ /;s/\r/ /;s/\t/ /:pane_title}"
        );
        let wrapped = fold_controls("#{pane_title}");
        assert!(
            !wrapped[..wrapped.rfind(':').unwrap()].contains("[["),
            "a POSIX class would silently expand to nothing: {wrapped}"
        );
    }

    /// A well-formed line for `parse_pane_lines`, with `display` as `@agent_display`.
    fn pane_line(pane_id: &str, pid: &str, display: &str) -> String {
        pane_line_with_alt(pane_id, pid, display, "0")
    }

    fn pane_line_with_alt(pane_id: &str, pid: &str, display: &str, alt: &str) -> String {
        let mut fields = vec![
            pane_id, "$0", "one", "/repo", "1", "@0", "0", "win", "1", "layout", "0", "1", pid,
            "zsh", "/repo", "title", "80", "24", "0", alt,
        ];
        assert_eq!(fields.len(), PANE_FIXED_FIELDS.len());
        let mut options = vec![""; crate::store::OPTION_FORMATS.len()];
        options[8] = display; // @agent_display
        fields.extend(options);
        fields.join("\t")
    }

    /// [`pane_line`] for a pane tmux says `dead` (`#{pane_dead}`) of.
    fn dead_pane_line(pane_id: &str, pid: &str, dead: &str) -> String {
        let line = pane_line(pane_id, pid, "x");
        let mut fields: Vec<&str> = line.split('\t').collect();
        fields[18] = dead;
        fields.join("\t")
    }

    /// tmux 3.8 prints `#{pane_pid}` empty for a pane kept after its program ended, where 3.6b printed
    /// the old pid. The pane stays in the listing, with no process (0); a live pane with an empty pid is
    /// still unreadable, and a dead pane with a pid (3.6b) keeps it.
    #[test]
    fn a_dead_pane_with_no_pid_is_listed_with_no_process_and_a_live_one_is_not() {
        let out = [
            dead_pane_line("%1", "", "1"),
            dead_pane_line("%2", "", "0"),
            dead_pane_line("%3", "300", "1"),
            dead_pane_line("%4", "x", "1"),
        ]
        .join("\n");
        let (panes, skipped) = parse_pane_lines(&out, None);
        let seen: Vec<(&str, u32, bool)> = panes
            .iter()
            .map(|p| (p.pane_id.as_str(), p.pane_pid, p.pane_dead))
            .collect();
        assert_eq!(seen, [("%1", 0, true), ("%3", 300, true)]);
        assert_eq!(
            skipped, 2,
            "an empty pid on a live pane, and a pid that is no number"
        );
    }

    #[test]
    fn alternate_on_is_read_as_the_twentieth_fixed_field() {
        let line = pane_line_with_alt("%9", "900", "x", "1");
        let pane = parse_pane_line(&line, None).expect("parses");
        assert!(
            pane.alternate_on,
            "1 means the pane is in the alternate screen"
        );
        let line = pane_line_with_alt("%9", "900", "x", "0");
        assert!(!parse_pane_line(&line, None).expect("parses").alternate_on);
    }

    #[test]
    fn one_unreadable_pane_costs_its_own_record_and_not_the_listing() {
        let good = pane_line("%1", "100", "first");
        let bad = pane_line("%2", "not-a-pid", "second");
        let short = "%3\ttoo\tfew";
        let out = format!("{good}\n{bad}\n{short}\n{}", pane_line("%4", "400", "last"));

        let (panes, skipped) = parse_pane_lines(&out, None);
        let ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(
            ids,
            ["%1", "%4"],
            "the two unreadable rows drop out; every other pane survives"
        );
        assert_eq!(skipped, 2, "and the caller is told how many were lost");
        assert_eq!(panes[0].agent.display.as_deref(), Some("first"));
        assert_eq!(panes[1].agent.display.as_deref(), Some("last"));
    }

    #[test]
    fn an_empty_listing_parses_as_no_panes() {
        let (panes, skipped) = parse_pane_lines("", None);
        assert!(panes.is_empty());
        assert_eq!(skipped, 0);
    }

    #[test]
    fn what_follows_the_panes_in_one_invocation_is_split_off_at_the_mark() {
        let (one, two) = (pane_line("%1", "100", "a"), pane_line("%2", "200", "b"));
        let out = format!("{one}\n{two}\n{PANES_END}\n%3 %5 \n");
        let (listing, answer) = split_at_panes_end(&out);
        assert_eq!(listing, format!("{one}\n{two}\n"));
        assert_eq!(answer, "%3 %5 \n");
        assert_eq!(parse_pane_lines(listing, None).0.len(), 2);
    }

    #[test]
    fn nothing_after_the_mark_is_an_empty_answer_and_no_mark_is_all_listing() {
        let row = pane_line("%1", "100", "a");
        let out = format!("{row}\n{PANES_END}\n");
        assert_eq!(split_at_panes_end(&out), (format!("{row}\n").as_str(), ""));
        let out = format!("{row}\n");
        assert_eq!(split_at_panes_end(&out), (out.as_str(), ""));
        assert_eq!(split_at_panes_end(""), ("", ""));
    }

    /// A row that holds the mark's words (a title, a name) is still a row: the mark is a line of
    /// its own, and every row has a tab between its fields.
    #[test]
    fn a_row_that_holds_the_words_of_the_mark_is_not_the_mark() {
        let said = pane_line("%1", "100", PANES_END);
        let out = format!("{said}\n{PANES_END}\n%4 \n");
        let (listing, answer) = split_at_panes_end(&out);
        assert_eq!(listing, format!("{said}\n"));
        assert_eq!(answer, "%4 \n");
        let (panes, _) = parse_pane_lines(listing, None);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].agent.display.as_deref(), Some(PANES_END));
    }
}
