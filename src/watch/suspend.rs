//! The agent a pane records, suspended with Ctrl+Z (herdr v0.9.4's suspend): the process a read found
//! it running as, in front of the pane, is noted, so that a later read can tell an agent stopped (or
//! run on in the background) behind the pane's shell from one that has exited.
//!
//! The note lives in the watcher and on the pane ([`crate::store::PROCESS_OPTION`]), so that a watcher
//! started while the agent is stopped (a restart after an update, a plugin reload) still knows it.

use std::time::{Duration, Instant};

/// How often, at most, the process table is asked about the agent's process, in front of the pane
/// (to note it again, so that a same-named program that took its place is the one noted) and behind
/// it (whether it is still there), unless the pane's command changed since the last look.
pub const LOOK_EVERY: Duration = Duration::from_secs(1);

/// What `ps` names a process by on macOS (`ucomm`, the kernel's `p_comm`) holds at most this many
/// bytes, where tmux names the process by its longer `pbi_name`.
const UCOMM_BYTES: usize = 16;

/// The process a read found the pane's agent running as: its pid, the kind of agent the pane recorded
/// for it, the command tmux named it by, which tells a pid the system gave to another program, and the
/// session the pane records for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcess {
    pub pid: u32,
    /// The agent's kind. An agent found through its job or a container runs as `bash`, `node` or
    /// `docker`, which `identify` does not name by the command. `None` for a note the pane kept from
    /// before the kind was in it.
    pub kind: Option<String>,
    pub command: String,
    pub session: Option<String>,
}

impl AgentProcess {
    /// The value of the pane option that keeps the note: the pid, the kind and the command, each after
    /// a space (a note with no kind, the older shape, is the pid and the command).
    pub fn option_value(&self) -> String {
        match &self.kind {
            Some(kind) => format!("{} {kind} {}", self.pid, self.command),
            None => format!("{} {}", self.pid, self.command),
        }
    }

    /// The note [`AgentProcess::option_value`] wrote, for the agent of the session `session`.
    pub fn parse(value: &str, session: Option<&str>) -> Option<AgentProcess> {
        let (pid, rest) = value.trim().split_once(' ')?;
        let pid = pid.parse::<u32>().ok().filter(|pid| *pid != 0)?;
        // A kind is one word and a command can have spaces, so a note that has two words after the
        // pid or more is the new shape, and one word is the older.
        let (kind, command) = match rest.trim_start().split_once(' ') {
            Some((kind, command)) if !command.trim().is_empty() => {
                (Some(kind.to_string()), command.trim_start())
            }
            _ => (None, rest.trim()),
        };
        (!command.is_empty()).then(|| AgentProcess {
            pid,
            kind,
            command: command.to_string(),
            session: session.map(str::to_string),
        })
    }
}

/// How the note looks at the pane's processes; the watcher's asks tmux and `ps`.
pub trait Look {
    /// The leader of the pane terminal's foreground process group: the process tmux names.
    fn foreground(&mut self) -> Option<u32>;
    /// The pane's own first process, its shell.
    fn shell(&mut self) -> Option<u32>;
    /// What `ps -o pid=,pgid=,tpgid=,tty=,stat=,ucomm= -p <pids>` printed.
    fn ps(&mut self, pids: &str) -> Option<String>;
}

/// What a read found that the note goes by.
#[derive(Debug, Clone, Copy)]
pub struct Seen<'a> {
    /// The agent the pane records.
    pub kind: Option<&'a str>,
    /// Whether that agent is the one in front of the pane.
    pub in_front: bool,
    /// The command tmux names in front of the pane.
    pub command: &'a str,
    /// The session the pane records.
    pub session: Option<&'a str>,
}

/// What a read of the note says: whether the agent is suspended, and whether the noted process
/// changed, so that the pane's copy is to be written again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Read {
    pub suspended: bool,
    pub changed: bool,
}

/// The agent's process, noted while it is in front of the pane.
#[derive(Debug, Default)]
pub struct Note {
    process: Option<AgentProcess>,
    /// Whether the last read found the agent in front.
    in_front: bool,
    /// When the process table was last asked, and under which command.
    looked: Option<(Instant, String)>,
    /// What that look found out of front: the process alive behind the shell.
    behind: bool,
}

impl Note {
    pub fn process(&self) -> Option<&AgentProcess> {
        self.process.as_ref()
    }

    /// The first read of a watcher: the note the pane kept (`saved`) is taken back only if its process
    /// is alive, stopped behind the pane's shell, and named as the agent the pane records. Returns
    /// whether the pane's copy is to go.
    pub fn start(
        &mut self,
        saved: Option<&str>,
        seen: &Seen<'_>,
        now: Instant,
        look: &mut impl Look,
    ) -> bool {
        let Some(saved) = saved else {
            return false;
        };
        let kept = AgentProcess::parse(saved, seen.session).filter(|process| {
            seen.kind.is_some()
                && match &process.kind {
                    Some(kind) => Some(kind.as_str()) == seen.kind,
                    // The older note has none: the command's name says what it was.
                    None => crate::detect::identify(&process.command, None).as_deref() == seen.kind,
                }
                && behind(look, process) == Some(true)
        });
        let Some(process) = kept else {
            return true;
        };
        ::log::info!(
            "its agent's process {}, noted before this watcher started, is stopped behind its shell",
            process.pid
        );
        self.process = Some(process);
        self.looked = Some((now, seen.command.to_string()));
        self.behind = true;
        false
    }

    /// One read: notes the agent's process while it is in front (at once when it comes to the front,
    /// then at most once a [`LOOK_EVERY`]), and says whether it is suspended once it is not.
    pub fn read(&mut self, seen: &Seen<'_>, now: Instant, look: &mut impl Look) -> Read {
        if seen.kind.is_none() {
            self.in_front = false;
            self.behind = false;
            return Read {
                suspended: false,
                changed: self.process.take().is_some(),
            };
        }
        let was_in_front = std::mem::replace(&mut self.in_front, seen.in_front);
        if seen.in_front {
            self.behind = false;
            let mut changed = false;
            if !was_in_front || self.due(now, seen.command) {
                self.looked = Some((now, seen.command.to_string()));
                let pid = look.foreground();
                let shell = look.shell();
                // A Ctrl+Z landing between tmux naming the agent and this look leaves the shell's
                // group in front: that is not the agent's process.
                if let Some(pid) = pid.filter(|pid| Some(*pid) != shell)
                    && self.process.as_ref().is_none_or(|process| {
                        process.pid != pid
                            || process.command != seen.command
                            || process.kind.as_deref() != seen.kind
                    })
                {
                    self.process = Some(AgentProcess {
                        pid,
                        kind: seen.kind.map(str::to_string),
                        command: seen.command.to_string(),
                        session: None,
                    });
                    changed = true;
                }
            }
            // While it is in front, the session the pane records is the agent's own, whatever
            // `/clear` or `/resume` made of it.
            if let Some(process) = &mut self.process {
                process.session = seen.session.map(str::to_string);
            }
            return Read {
                suspended: false,
                changed,
            };
        }
        let Some(process) = &self.process else {
            self.behind = false;
            return Read::default();
        };
        if !same_agent(process.session.as_deref(), seen.session) {
            return self.ended();
        }
        if was_in_front || self.due(now, seen.command) {
            self.looked = Some((now, seen.command.to_string()));
            if behind(look, process).is_none() {
                return self.ended();
            }
            self.behind = true;
        }
        Read {
            suspended: self.behind,
            changed: false,
        }
    }

    fn ended(&mut self) -> Read {
        self.process = None;
        self.behind = false;
        Read {
            suspended: false,
            changed: true,
        }
    }

    /// Whether the process table is to be asked again: the command changed since the last look, or
    /// [`LOOK_EVERY`] has passed.
    fn due(&self, now: Instant, command: &str) -> bool {
        self.looked.as_ref().is_none_or(|(at, looked)| {
            looked != command || now.saturating_duration_since(*at) >= LOOK_EVERY
        })
    }
}

/// Whether `process` is behind the pane's shell, by one look: `Some(true)` stopped, `Some(false)` run
/// on in the background, `None` neither (gone, another program, in front, on another terminal).
fn behind(look: &mut impl Look, process: &AgentProcess) -> Option<bool> {
    let shell = look
        .shell()
        .filter(|shell| *shell != 0 && *shell != process.pid)?;
    let ps = look.ps(&format!("{},{shell}", process.pid))?;
    behind_the_panes_shell(&ps, process.pid, shell, &process.command)
}

/// Whether the session the pane records now is still the one of the agent whose process was noted
/// with `recorded`: another agent that reported a session in the pane has taken it (herdr lets the
/// kept session go when another agent takes over). A session reported where none was is the agent's
/// own.
pub fn same_agent(recorded: Option<&str>, now: Option<&str>) -> bool {
    !matches!((recorded, now), (Some(recorded), Some(now)) if recorded != now)
}

/// Whether `ps` names a process `ucomm` that tmux names `command`: the same, or on macOS, where `ps`
/// keeps only the first [`UCOMM_BYTES`] bytes, those.
fn same_name(ucomm: &str, command: &str) -> bool {
    ucomm == command
        || (cfg!(target_os = "macos")
            && command.len() > UCOMM_BYTES
            && command.as_bytes().get(..UCOMM_BYTES) == Some(ucomm.as_bytes()))
}

/// By what `ps -o pid=,pgid=,tpgid=,tty=,stat=,ucomm= -p <pid>,<shell>` printed, whether the process
/// `pid` is alive behind the pane's shell `shell`: on the same terminal, still the program `command`
/// names, and stopped (`T`, `Some(true)`) or in a process group that is not the one in front of the
/// terminal (`Some(false)`).
pub fn behind_the_panes_shell(
    ps_output: &str,
    pid: u32,
    shell: u32,
    command: &str,
) -> Option<bool> {
    let row = |wanted: u32| {
        ps_output.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()?.parse::<u32>().ok()? != wanted {
                return None;
            }
            let group: i64 = fields.next()?.parse().ok()?;
            let foreground: i64 = fields.next()?.parse().ok()?;
            let tty = fields.next()?.to_string();
            let stat = fields.next()?.to_string();
            let name = fields.collect::<Vec<_>>().join(" ");
            Some((group, foreground, tty, stat, name))
        })
    };
    let (group, foreground, tty, stat, name) = row(pid)?;
    let (_, _, shell_tty, _, _) = row(shell)?;
    let on_a_terminal = !tty.is_empty() && !tty.starts_with('?');
    let stopped = stat.starts_with('T');
    (on_a_terminal
        && tty == shell_tty
        && same_name(&name, command)
        && (stopped || group != foreground))
        .then_some(stopped)
}

#[cfg(test)]
mod tests;
