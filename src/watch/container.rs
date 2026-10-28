//! The agent in the container runtime in front of a pane, as the watcher finds it.
//!
//! [`crate::detect::container`] says which agent a runtime's arguments name. This is where
//! the arguments come from and how often they are asked for: the arguments of the process
//! in front of the pane are read once for each process, when its name is a container
//! runtime's, so a container that runs for days costs at most one `ps` call a second to find
//! out who is in front and one read, the first time, for what it was started with.
//!
//! The process is the one tmux names. `#{pane_current_command}` is the name of the leader
//! of the terminal's foreground process group, so that leader's pid is read off the pane's
//! shell (`ps -o tpgid=`). Its arguments are read exactly, one string for each ([`argv`]),
//! and only when that cannot be done is the line `ps -o args=` printed for it taken, as
//! words. Nothing here logs or keeps the arguments: they can carry credentials
//! (`-e TOKEN=...`), and what is kept is the agent's id.

mod argv;
mod tpgid;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::detect::{self, ContainerAgent};
use crate::tmux::Tmux;

/// How many reads of one process that could not be taken are tried, in a row, before it is
/// taken as a process that names no agent.
pub const ATTEMPTS: u8 = 5;

/// The least time between two askings of which process is in front of the pane. That is a
/// `ps` call, and a pane whose agent is working is looked at several times a second.
const PID_CHECK: Duration = Duration::from_secs(1);

/// The arguments of a process, as they were read.
#[derive(Clone, Debug)]
pub enum Args {
    /// The exact list, one string for each argument.
    Exact(Vec<String>),
    /// The line `ps` printed for them, when the list could not be had.
    Printed(String),
}

impl Args {
    /// The agent the container runtime `command` starts with these arguments.
    pub(super) fn agent(&self, command: &str) -> Option<ContainerAgent> {
        match self {
            Args::Exact(argv) => detect::identify_container_argv(command, argv),
            Args::Printed(line) => detect::identify_container_ps(command, line),
        }
    }
}

/// What the watcher of one pane knows about the container runtime in front of it.
#[derive(Debug)]
pub struct Probe {
    /// How long a process whose arguments could not be read is left before it is tried
    /// again: the watcher's identity recheck.
    recheck: Duration,
    /// What the last look at a runtime found, `None` when what was in front was none.
    last: Option<Last>,
}

/// What the last look at the runtime `command` found.
#[derive(Debug)]
struct Last {
    command: String,
    /// The process that was in front, `None` when it could not be found.
    pid: Option<u32>,
    stand: Stand,
    /// When which process was in front was last asked.
    asked: Instant,
}

/// Where the reading of a process stands.
#[derive(Debug)]
enum Stand {
    /// Its arguments were read, and name this.
    Read(Option<ContainerAgent>),
    /// So many reads in a row could not be taken, fewer than [`ATTEMPTS`].
    Failing(u8),
    /// [`ATTEMPTS`] in a row could not: it is taken as naming no agent until it is tried again.
    GaveUp,
}

impl Stand {
    fn agent(&self) -> Option<ContainerAgent> {
        match self {
            Stand::Read(agent) => agent.clone(),
            Stand::Failing(_) | Stand::GaveUp => None,
        }
    }
}

/// What one look at the pane came to.
#[derive(Debug)]
pub struct Look {
    /// The agent the runtime in front is running, if any.
    pub agent: Option<ContainerAgent>,
    /// This look gave up on reading the process, after [`ATTEMPTS`] failures in a row, and
    /// says so once for it: the next tries, at every recheck, stay silent.
    pub gave_up: bool,
}

impl Probe {
    /// A probe that tries a process it gave up on again after `recheck`.
    pub fn new(recheck: Duration) -> Probe {
        Probe {
            recheck,
            last: None,
        }
    }

    /// What the container runtime that is the pane's foreground `command` is running, at
    /// `now`, or no agent when `command` is no runtime or its arguments name none.
    ///
    /// The arguments belong to a process, not to a name: `docker run a claude; docker run b
    /// codex` is `docker` throughout, with a shell that is never in front of the pane long
    /// enough to be seen. So `pid` says which process is in front, and `read` fetches its
    /// arguments only when it is not the one already read. `pid` is asked at most once a
    /// second ([`PID_CHECK`]), and the answer given between is the last one. A read that finds
    /// nothing (the process had gone, `ps` would not say) is not kept, and is tried again, up
    /// to [`ATTEMPTS`] times in a row, and then once for each recheck. Another command in
    /// between, the shell's included, forgets what was read, so the next run of `docker` is
    /// another run even under a pid the last one had.
    pub fn agent(
        &mut self,
        command: &str,
        now: Instant,
        pid: impl FnOnce() -> Option<u32>,
        read: impl FnOnce(u32) -> Option<Args>,
    ) -> Look {
        if detect::container_runtime(command).is_none() {
            self.last = None;
            return Look {
                agent: None,
                gave_up: false,
            };
        }
        // Not yet time to ask which process is in front: it is the one that was.
        if let Some(last) = self.last.as_ref().filter(|last| last.command == command) {
            let wait = match last.stand {
                Stand::GaveUp => self.recheck,
                Stand::Read(_) | Stand::Failing(_) => PID_CHECK,
            };
            if now.saturating_duration_since(last.asked) < wait {
                return Look {
                    agent: last.stand.agent(),
                    gave_up: false,
                };
            }
        }
        let pid = pid();
        // What was found of this same process, if it is one.
        let before = self
            .last
            .take()
            .filter(|last| last.command == command && last.pid == pid)
            .map(|last| last.stand);
        let (stand, gave_up) = match (before, pid) {
            // The process that was read is still in front.
            (Some(read @ Stand::Read(_)), Some(_)) => (read, false),
            (before, pid) => match pid.and_then(read) {
                Some(args) => (Stand::Read(args.agent(command)), false),
                None => match before {
                    Some(Stand::GaveUp) => (Stand::GaveUp, false),
                    Some(Stand::Failing(tries)) if tries + 1 >= ATTEMPTS => (Stand::GaveUp, true),
                    Some(Stand::Failing(tries)) => (Stand::Failing(tries + 1), false),
                    Some(Stand::Read(_)) | None => (Stand::Failing(1), false),
                },
            },
        };
        let agent = stand.agent();
        self.last = Some(Last {
            command: command.to_string(),
            pid,
            stand,
            asked: now,
        });
        Look { agent, gave_up }
    }

    /// Whether the runtime in front has not been read yet, or was given up on: a pane that
    /// may be running an agent, and has to be looked at again although nothing may print.
    pub fn wants_recheck(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|last| !matches!(last.stand, Stand::Read(_)))
    }
}

/// The process in front of a pane, found from the pane's own first process.
#[derive(Debug, Default)]
pub struct Foreground {
    /// That first process, the pane's shell. Its pid is fixed for as long as the pane runs
    /// it, so tmux is asked for it once, and again only when `ps` no longer knows it: the
    /// pane was respawned.
    shell: Option<u32>,
}

impl Foreground {
    /// The pid of the process in front of `pane`: the leader of the terminal's foreground
    /// process group, which is the process tmux names.
    pub fn pid(&mut self, t: &Tmux, pane: &str) -> Option<u32> {
        if let Some(shell) = self.shell
            && let Some(group) = foreground_of(shell)
        {
            return group;
        }
        let shell = t
            .display(pane, "#{pane_pid}")
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        self.shell = Some(shell);
        foreground_of(shell)?
    }

    /// The pane's own first process, its shell, as [`Foreground::pid`] last found it: `None` until that
    /// has been asked.
    pub fn shell(&self) -> Option<u32> {
        self.shell
    }
}

/// The foreground process group of the terminal the process `shell` is on: `None` when the process
/// is not known, `Some(None)` when it has no terminal. The system is asked ([`tpgid::of`]), which
/// starts no process, and `ps` only when the system could not be, or when a test asks for it
/// ([`FOREGROUND_BY_PS_ENV`]).
///
/// `shell` 0 is a pane with no process (a pane kept after its program ended, which tmux 3.8 gives no
/// pid): `None` without asking, since process 0 is the kernel's own.
pub(super) fn foreground_of(shell: u32) -> Option<Option<u32>> {
    foreground_with(
        shell,
        by_ps_from(std::env::var_os(FOREGROUND_BY_PS_ENV)),
        tpgid::of,
        |shell| ps(&["-o", "tpgid=", "-p", shell]),
    )
}

/// Set to anything but empty in the watcher's environment, the foreground group is read with `ps`
/// only, never from the system, for a test: it reaches the `ps` fallback on purpose, as a system read
/// that failed does, so a test whose `ps` refuses can make the look fail.
const FOREGROUND_BY_PS_ENV: &str = "TAW_FOREGROUND_BY_PS";

/// Whether the value of [`FOREGROUND_BY_PS_ENV`] asks for `ps`: set, and not empty.
fn by_ps_from(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// [`foreground_by`], with the system not asked at all when `by_ps` ([`FOREGROUND_BY_PS_ENV`]).
fn foreground_with(
    shell: u32,
    by_ps: bool,
    system_of: impl FnOnce(u32) -> tpgid::Tpgid,
    ps_of: impl FnOnce(&str) -> Option<String>,
) -> Option<Option<u32>> {
    foreground_by(
        shell,
        |shell| {
            if by_ps {
                tpgid::Tpgid::Failed
            } else {
                system_of(shell)
            }
        },
        ps_of,
    )
}

/// [`foreground_of`] with the two looks given: `system_of` is asked first, and `ps_of` only when it
/// failed; neither for 0.
fn foreground_by(
    shell: u32,
    system_of: impl FnOnce(u32) -> tpgid::Tpgid,
    ps_of: impl FnOnce(&str) -> Option<String>,
) -> Option<Option<u32>> {
    if shell == 0 {
        return None;
    }
    match system_of(shell) {
        tpgid::Tpgid::Group(group) => return Some(group),
        tpgid::Tpgid::Gone => return None,
        tpgid::Tpgid::Failed => {}
    }
    let out = ps_of(&shell.to_string())?;
    Some(out.trim().parse::<u32>().ok().filter(|group| *group > 0))
}

/// The arguments of the process `pid`, when it is the runtime `command` names: the exact
/// list, or the line `ps` prints for them when the list cannot be had. `None` when they
/// cannot be read or the process is another one by now.
pub fn read_args(pid: u32, command: &str) -> Option<Args> {
    args_of(pid, command, argv::of, command_line_of)
}

/// [`read_args`] with the two readings given. The list is asked for first, and `ps` only
/// when it is not there: a process that was read exactly and is not the runtime is another
/// process, whatever `ps` would say.
fn args_of(
    pid: u32,
    command: &str,
    exact: impl FnOnce(u32) -> Option<Vec<String>>,
    printed: impl FnOnce(u32) -> Option<String>,
) -> Option<Args> {
    if let Some(argv) = exact(pid) {
        return argv
            .first()
            .is_some_and(|program| names_runtime(program, command))
            .then_some(Args::Exact(argv));
    }
    let line = printed(pid)?;
    let program = line.split_whitespace().next()?;
    names_runtime(program, command).then_some(Args::Printed(line))
}

/// The exact arguments of the process `pid`, one string each, or `None` when they cannot be read
/// ([`argv::of`]).
pub(super) fn exact_args_of(pid: u32) -> Option<Vec<String>> {
    argv::of(pid)
}

/// The whole command line of a process, as `ps` prints it. `-ww` is what stops `ps` cutting
/// it to the width of a terminal, which it does on some systems even when it is writing to a
/// pipe.
pub(super) fn command_line_of(pid: u32) -> Option<String> {
    let line = ps(&["-ww", "-o", "args=", "-p", &pid.to_string()])?;
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Whether `program`, the first word of a process's arguments, is the container runtime
/// `command` names: read a moment after tmux named the command, the process in front may be
/// another. Both are compared as what they are recorded as, so `docker` is the runtime
/// Docker Desktop's `com.docker.cli` names.
fn names_runtime(program: &str, command: &str) -> bool {
    detect::container_runtime(program)
        .is_some_and(|runtime| Some(runtime) == detect::container_runtime(command))
}

/// What `ps` prints, or `None` when it cannot run or exits with an error (no such process).
fn ps(args: &[&str]) -> Option<String> {
    let out = Command::new("ps")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use super::*;

    /// A shell function that runs Claude in a container, as `ps` prints it, less most of its arguments.
    const BOXED: &str = "docker run --rm -it -v /a/project:/a/project --entrypoint bash dev-image -lc /usr/local/bin/firewall.sh || true; eval \"$(mise activate bash)\"; claude --effort medium _";
    const CODEX: &str = "docker run --rm -it -v /src:/src dev-image codex";
    const PS: &str = "docker ps -a";
    const RECHECK: Duration = Duration::from_secs(5);
    const SECOND: Duration = Duration::from_secs(1);

    /// The process in front of a pane, as a look at it finds it: which pid is in front, and
    /// the arguments each pid has, counting how often either is asked for, with a clock that
    /// moves `step` at every look.
    struct Front {
        now: Cell<Instant>,
        step: Cell<Duration>,
        pid: Cell<Option<u32>>,
        lines: RefCell<HashMap<u32, Args>>,
        pids_asked: Cell<u32>,
        lines_asked: Cell<u32>,
        gave_up: Cell<u32>,
    }

    impl Default for Front {
        fn default() -> Front {
            Front {
                now: Cell::new(Instant::now()),
                step: Cell::new(SECOND),
                pid: Cell::default(),
                lines: RefCell::default(),
                pids_asked: Cell::default(),
                lines_asked: Cell::default(),
                gave_up: Cell::default(),
            }
        }
    }

    impl Front {
        /// `pid` is in front, running `line`, which is all `ps` says of it.
        fn run(&self, pid: u32, line: &str) {
            self.pid.set(Some(pid));
            self.lines
                .borrow_mut()
                .insert(pid, Args::Printed(line.to_string()));
        }

        /// `pid` is in front, running a program with exactly these arguments.
        fn run_argv(&self, pid: u32, argv: Vec<String>) {
            self.pid.set(Some(pid));
            self.lines.borrow_mut().insert(pid, Args::Exact(argv));
        }

        /// Time passes.
        fn wait(&self, time: Duration) {
            self.now.set(self.now.get() + time);
        }

        /// One look at the pane, a step after the last, while `command` is what tmux names.
        fn look(&self, probe: &mut Probe, command: &str) -> Option<ContainerAgent> {
            self.wait(self.step.get());
            let look = probe.agent(
                command,
                self.now.get(),
                || {
                    self.pids_asked.set(self.pids_asked.get() + 1);
                    self.pid.get()
                },
                |pid| {
                    self.lines_asked.set(self.lines_asked.get() + 1);
                    self.lines.borrow().get(&pid).cloned()
                },
            );
            self.gave_up
                .set(self.gave_up.get() + u32::from(look.gave_up));
            look.agent
        }

        fn pids_asked(&self) -> u32 {
            self.pids_asked.get()
        }

        fn lines_asked(&self) -> u32 {
            self.lines_asked.get()
        }

        /// How many looks said they gave up.
        fn gave_up(&self) -> u32 {
            self.gave_up.get()
        }
    }

    fn kind(found: Option<ContainerAgent>) -> Option<String> {
        found.map(|found| found.kind)
    }

    #[test]
    fn the_arguments_are_read_once_for_as_long_as_the_process_stays() {
        let front = Front::default();
        front.run(7, BOXED);
        let mut probe = Probe::new(RECHECK);
        for _ in 0..5 {
            let found = front.look(&mut probe, "docker").unwrap();
            assert_eq!((found.runtime, found.kind.as_str()), ("docker", "claude"));
        }
        assert_eq!(front.lines_asked(), 1);
        // The pid is asked for at every look a second apart: it is what says the process is
        // the same.
        assert_eq!(front.pids_asked(), 5);
    }

    #[test]
    fn which_process_is_in_front_is_asked_at_most_once_a_second() {
        // A working agent's pane is looked at several times a second, and each asking is a
        // `ps` call: 35 looks a tenth of a second apart ask four times, at the first look and
        // then when a second is up.
        let front = Front::default();
        front.step.set(Duration::from_millis(100));
        front.run(7, BOXED);
        let mut probe = Probe::new(RECHECK);
        for _ in 0..35 {
            assert_eq!(
                kind(front.look(&mut probe, "docker")).as_deref(),
                Some("claude")
            );
        }
        assert_eq!(front.pids_asked(), 4);
        assert_eq!(front.lines_asked(), 1);
    }

    #[test]
    fn a_process_that_takes_the_pane_is_found_within_a_second() {
        let front = Front::default();
        front.step.set(Duration::from_millis(100));
        front.run(7, BOXED);
        let mut probe = Probe::new(RECHECK);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        front.run(8, CODEX);
        let mut looks = 0;
        while kind(front.look(&mut probe, "docker")).as_deref() != Some("codex") {
            looks += 1;
            assert!(looks <= 10, "still the first process after a second");
        }
        // Another command is another matter, and is never waited on.
        front.run(9, "podman run img gemini");
        assert_eq!(
            kind(front.look(&mut probe, "podman")).as_deref(),
            Some("gemini")
        );
        assert_eq!(front.look(&mut probe, "zsh"), None);
    }

    #[test]
    fn a_new_process_under_the_same_command_is_read_again() {
        // `docker run a claude; docker run b codex`: the shell hands the pane from one docker
        // to the next with no prompt between, so the command is `docker` throughout.
        let front = Front::default();
        let mut probe = Probe::new(RECHECK);
        front.run(7, BOXED);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        front.run(8, CODEX);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("codex")
        );
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("codex")
        );
        assert_eq!(front.lines_asked(), 2);
        // A run that names no agent takes the pane over the same way.
        front.run(9, PS);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.lines_asked(), 3);
    }

    #[test]
    fn a_command_that_is_no_runtime_is_never_looked_at() {
        let front = Front::default();
        front.run(7, BOXED);
        let mut probe = Probe::new(RECHECK);
        for command in ["zsh", "claude", "vim", ""] {
            assert_eq!(front.look(&mut probe, command), None, "{command}");
        }
        assert_eq!((front.pids_asked(), front.lines_asked()), (0, 0));
        assert!(!probe.wants_recheck());
    }

    #[test]
    fn a_runtime_that_runs_no_agent_is_remembered_as_that_for_the_process() {
        let front = Front::default();
        front.run(3, PS);
        let mut probe = Probe::new(RECHECK);
        for _ in 0..3 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!(front.lines_asked(), 1);
        assert!(!probe.wants_recheck(), "read, and it names no agent");
    }

    #[test]
    fn a_new_command_is_read_again_and_the_shell_between_two_runs_is_forgotten() {
        let front = Front::default();
        let mut probe = Probe::new(RECHECK);
        front.run(7, BOXED);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        // Another runtime in the pane.
        front.run(8, "podman run -it img codex");
        assert_eq!(
            kind(front.look(&mut probe, "podman")).as_deref(),
            Some("codex")
        );
        assert_eq!(front.lines_asked(), 2);
        // The container stops and the shell is back. The next docker is a different run even
        // when the system gives it the pid the last one had.
        assert_eq!(front.look(&mut probe, "zsh"), None);
        front.run(8, PS);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.lines_asked(), 3);
        front.run(8, BOXED);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(
            front.lines_asked(),
            3,
            "the last read stands while the process does"
        );
        // The same runtime again after the shell, and under the pid the last run had: the
        // shell in between forgot that run, so this one is read.
        front.run(7, BOXED);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!(front.look(&mut probe, "zsh"), None);
        front.run(7, PS);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.lines_asked(), 5);
    }

    #[test]
    fn a_read_that_found_nothing_to_read_is_asked_for_again_at_the_next_look() {
        let front = Front::default();
        front.pid.set(Some(7));
        let mut probe = Probe::new(RECHECK);
        // The process is in front, and `ps` has nothing for it yet.
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.lines_asked(), 2);
        assert!(probe.wants_recheck(), "not read yet");
        front.run(7, BOXED);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!(front.lines_asked(), 3);
        assert!(!probe.wants_recheck());
        front.lines.borrow_mut().clear();
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!(
            front.lines_asked(),
            3,
            "a read that succeeded is not asked for again"
        );
    }

    #[test]
    fn failing_reads_are_no_closer_together_than_the_pid_is_asked() {
        // Five tries in a row, and each asks for the pid: a second apart at the least.
        let front = Front::default();
        front.step.set(Duration::from_millis(100));
        front.pid.set(Some(7));
        let mut probe = Probe::new(RECHECK);
        let mut gave_up_at = None;
        for look in 1..=50 {
            assert_eq!(front.look(&mut probe, "docker"), None);
            if gave_up_at.is_none() && front.gave_up() == 1 {
                gave_up_at = Some(look);
            }
        }
        assert_eq!(front.lines_asked(), 5);
        assert_eq!(front.gave_up(), 1);
        // The first try is at the first look and each of the next four a second on, so the
        // fifth is at the forty-first look of a tenth of a second each.
        assert_eq!(gave_up_at, Some(41));
    }

    #[test]
    fn a_read_that_keeps_failing_is_given_up_on_after_five_tries_and_said_once() {
        // A `ps` that will not say, or a process that is never the runtime tmux named: asked
        // for at every read of a pane that may print all the time, it would cost a `ps` or
        // two per tick for as long as the process stays. Five tries cover a process that was
        // only changing, and then it is taken as one that names no agent, and the log says so.
        let front = Front::default();
        front.pid.set(Some(7));
        let mut probe = Probe::new(RECHECK);
        for _ in 0..4 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!(front.gave_up(), 0);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!(front.gave_up(), 1);
        assert_eq!(front.lines_asked(), 5);
        assert!(probe.wants_recheck(), "given up on, and tried again later");
        // While it stays given up on, nothing is asked until the recheck.
        front.run(7, BOXED);
        for _ in 0..4 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!(front.lines_asked(), 5, "given up on until the recheck");
        // The recheck, five seconds after the last ask, finds another process in front, and
        // that one starts afresh whatever its pid.
        front.run(8, BOXED);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!(front.lines_asked(), 6);
        assert!(!probe.wants_recheck());
    }

    #[test]
    fn a_process_given_up_on_is_tried_again_at_the_recheck() {
        let front = Front::default();
        front.pid.set(Some(7));
        let mut probe = Probe::new(RECHECK);
        for _ in 0..5 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!((front.lines_asked(), front.gave_up()), (5, 1));
        // The recheck comes, and the process still cannot be read: it stays given up on, and
        // the log is not told again.
        front.wait(RECHECK);
        assert_eq!(front.look(&mut probe, "docker"), None);
        assert_eq!((front.lines_asked(), front.gave_up()), (6, 1));
        assert!(probe.wants_recheck());
        // The next one finds the arguments.
        front.run(7, BOXED);
        front.wait(RECHECK);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!(front.lines_asked(), 7);
        assert!(!probe.wants_recheck());
        // And a new run of failures says so again, once.
        front.run(8, "");
        front.lines.borrow_mut().clear();
        for _ in 0..5 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!(front.gave_up(), 2);
    }

    #[test]
    fn a_process_that_takes_the_pane_from_one_given_up_on_starts_afresh() {
        let front = Front::default();
        front.pid.set(Some(7));
        let mut probe = Probe::new(RECHECK);
        for _ in 0..5 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        front.run(8, BOXED);
        front.wait(RECHECK);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        // A different command gives up nothing of it either.
        front.pid.set(Some(9));
        assert_eq!(front.look(&mut probe, "podman"), None);
        assert_eq!(front.look(&mut probe, "podman"), None);
        assert_eq!(front.gave_up(), 1);
    }

    #[test]
    fn tries_are_counted_for_one_process_at_a_time() {
        let front = Front::default();
        let mut probe = Probe::new(RECHECK);
        front.pid.set(Some(7));
        for _ in 0..4 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        // Another process in between starts the count again.
        front.pid.set(Some(8));
        assert_eq!(front.look(&mut probe, "docker"), None);
        front.pid.set(Some(7));
        for _ in 0..4 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!(front.lines_asked(), 9);
        assert_eq!(front.gave_up(), 0);
        front.run(7, BOXED);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude"),
            "nine failures, but never five in a row of one process"
        );
    }

    #[test]
    fn a_pid_that_cannot_be_had_is_asked_for_five_times_and_then_at_the_recheck() {
        let front = Front::default();
        let mut probe = Probe::new(RECHECK);
        for _ in 0..5 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!((front.pids_asked(), front.lines_asked()), (5, 0));
        assert_eq!(front.gave_up(), 1);
        // With no pid to say when the process changes, nothing is asked for until the
        // recheck, and then once.
        front.run(7, BOXED);
        for _ in 0..3 {
            assert_eq!(front.look(&mut probe, "docker"), None);
        }
        assert_eq!((front.pids_asked(), front.lines_asked()), (5, 0));
        front.wait(RECHECK);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!((front.pids_asked(), front.lines_asked()), (6, 1));
        // Another command starts afresh.
        assert_eq!(front.look(&mut probe, "zsh"), None);
        assert!(!probe.wants_recheck());
    }

    #[test]
    fn a_process_is_kept_only_when_it_is_the_runtime_that_was_named() {
        // Read a moment after tmux named the command, the process in front may be another.
        assert!(names_runtime("docker", "docker"));
        assert!(names_runtime("/usr/local/bin/docker", "docker"));
        assert!(names_runtime("Docker.exe", "docker"));
        assert!(names_runtime("podman", "podman"));
        assert!(!names_runtime("podman", "docker"));
        assert!(!names_runtime("zsh", "docker"));
        assert!(!names_runtime("-zsh", "docker"));
        assert!(!names_runtime("", "docker"));
        assert!(!names_runtime("docker", "zsh"));
        // Docker Desktop's CLI is `docker` to what is recorded, whichever name each has.
        assert!(names_runtime("docker", "com.docker.cli"));
        assert!(names_runtime("com.docker.cli", "docker"));
        assert!(!names_runtime("podman", "com.docker.cli"));
    }

    /// The arguments of a process that is what tmux named, or is not.
    fn exact(argv: &[&str]) -> impl FnOnce(u32) -> Option<Vec<String>> {
        let argv: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
        move |_| Some(argv)
    }

    fn printed(line: &str) -> impl FnOnce(u32) -> Option<String> {
        let line = line.to_string();
        move |_| Some(line)
    }

    fn nothing<T>(_: u32) -> Option<T> {
        None
    }

    #[test]
    fn the_exact_arguments_are_read_first_and_only_what_was_named_is_kept() {
        let read = args_of(
            1,
            "docker",
            exact(&["docker", "run", "img", "claude"]),
            nothing,
        );
        assert!(
            matches!(read, Some(Args::Exact(argv)) if argv == ["docker", "run", "img", "claude"])
        );
        // Another process by now: the exact list says so, and `ps` is not asked to say
        // otherwise.
        assert!(
            args_of(1, "docker", exact(&["zsh", "-l"]), |_| -> Option<String> {
                panic!("ps was asked after the list was read")
            })
            .is_none()
        );
        assert!(args_of(1, "docker", exact(&["podman", "run", "img"]), nothing).is_none());
        // A list with nothing in it names nothing.
        assert!(args_of(1, "docker", exact(&[]), nothing).is_none());
        // A list that started one argument late, as an empty argv[0] makes one (see argv.rs),
        // does not start with the runtime either.
        assert!(
            args_of(
                1,
                "docker",
                exact(&["run", "img", "claude", "TOKEN=secret"]),
                nothing
            )
            .is_none()
        );
    }

    #[test]
    fn ps_is_asked_only_when_the_list_could_not_be_had() {
        let read = args_of(1, "docker", nothing, printed("docker run img codex"));
        assert!(matches!(read, Some(Args::Printed(line)) if line == "docker run img codex"));
        // The line is held to the same rule: it has to be the runtime that was named.
        assert!(args_of(1, "docker", nothing, printed("zsh -l")).is_none());
        assert!(args_of(1, "docker", nothing, printed("podman run img codex")).is_none());
        assert!(args_of(1, "docker", nothing, printed("")).is_none());
        assert!(args_of(1, "docker", nothing, printed("   ")).is_none());
        // And neither reading: nothing.
        assert!(args_of(1, "docker", nothing::<Vec<String>>, nothing::<String>).is_none());
    }

    #[test]
    fn a_value_with_a_space_is_read_right_from_the_list_and_not_from_the_line() {
        // The same process, read the two ways. `ps` joins the arguments, so the value's second
        // word is taken for the image; the list is not fooled.
        let front = Front::default();
        front.run_argv(
            7,
            ["docker", "run", "-e", "MSG=hello world", "img", "claude"]
                .map(String::from)
                .to_vec(),
        );
        let mut probe = Probe::new(RECHECK);
        assert_eq!(
            kind(front.look(&mut probe, "docker")).as_deref(),
            Some("claude")
        );
        assert_eq!(
            detect::identify_container_ps("docker", "docker run -e MSG=hello world img claude"),
            None
        );
    }

    #[test]
    fn the_foreground_group_is_asked_of_a_process_ps_knows_and_of_no_other() {
        // This process is known to `ps`, on a terminal or not; a pid nothing has is not.
        assert!(foreground_of(std::process::id()).is_some());
        assert_eq!(foreground_of(u32::MAX - 1), None);
    }

    /// A pane with no process (pid 0: a pane kept after its program ended, on tmux 3.8) has no
    /// foreground group, and neither the system nor `ps` is asked about the kernel's own process 0.
    #[test]
    fn the_foreground_group_of_a_pane_with_no_process_is_not_asked_of_ps() {
        let asked = std::cell::Cell::new(false);
        let none = foreground_by(
            0,
            |_| {
                asked.set(true);
                tpgid::Tpgid::Failed
            },
            |_| {
                asked.set(true);
                Some("7\n".to_string())
            },
        );
        assert_eq!(none, None);
        assert!(!asked.get(), "process 0 was asked about");
        assert_eq!(
            foreground_by(
                5,
                |_| tpgid::Tpgid::Failed,
                |pid| (pid == "5").then(|| "7\n".to_string())
            ),
            Some(Some(7))
        );
    }

    /// `TAW_FOREGROUND_BY_PS` set and not empty: the system is not asked and `ps` answers. Unset or
    /// empty: the system answers and `ps` is not started.
    #[test]
    fn the_foreground_by_ps_seam_skips_the_system_read() {
        assert!(by_ps_from(Some("1".into())));
        assert!(!by_ps_from(Some("".into())));
        assert!(!by_ps_from(None));

        let (system_calls, ps_calls) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
        let system_says_7 = |_: u32| {
            system_calls.set(system_calls.get() + 1);
            tpgid::Tpgid::Group(Some(7))
        };
        let ps_says_9 = |_: &str| {
            ps_calls.set(ps_calls.get() + 1);
            Some("9\n".to_string())
        };
        assert_eq!(
            foreground_with(5, true, system_says_7, ps_says_9),
            Some(Some(9))
        );
        assert_eq!((system_calls.get(), ps_calls.get()), (0, 1));
        assert_eq!(
            foreground_with(5, false, system_says_7, ps_says_9),
            Some(Some(7))
        );
        assert_eq!((system_calls.get(), ps_calls.get()), (1, 1));
    }

    /// The system answers, `ps` is not started: a counter on the `ps` seam stays 0 for a group, a
    /// terminal-less process and a process that is gone, and moves once for a system that failed.
    #[test]
    fn ps_is_not_started_when_the_system_read_works() {
        let ps_calls = std::cell::Cell::new(0);
        let ps_says_9 = |_: &str| {
            ps_calls.set(ps_calls.get() + 1);
            Some("9\n".to_string())
        };
        assert_eq!(
            foreground_by(5, |_| tpgid::Tpgid::Group(Some(7)), ps_says_9),
            Some(Some(7))
        );
        assert_eq!(
            foreground_by(5, |_| tpgid::Tpgid::Group(None), ps_says_9),
            Some(None)
        );
        assert_eq!(foreground_by(5, |_| tpgid::Tpgid::Gone, ps_says_9), None);
        assert_eq!(
            ps_calls.get(),
            0,
            "ps was started though the system answered"
        );
        assert_eq!(
            foreground_by(5, |_| tpgid::Tpgid::Failed, ps_says_9),
            Some(Some(9))
        );
        assert_eq!(ps_calls.get(), 1);
    }

    /// The system read gives what `ps -o tpgid=` prints, for a shell of the test's own on a
    /// pseudo-terminal (`script`, the stand-in terminal the other tests start): the shell leads the
    /// group in front of its terminal, and the two agree. It runs `sleep`, so the shell stays alive.
    #[test]
    fn the_system_read_of_the_foreground_group_agrees_with_ps_on_a_terminal() {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("taw-tpgid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        // `; :` keeps the shell from replacing itself by the last command, so the shell is the one on
        // the terminal and stays.
        let line = format!("echo $$ > '{}'; sleep 8; :", pid_file.display());
        let Ok(mut child) = crate::watch::tests_support::script_command(&line)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            eprintln!("skipped: no `script` to give a shell a terminal");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let started = Instant::now();
        let mut shell = None;
        while started.elapsed() < Duration::from_secs(5) {
            shell = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            if shell.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let by_system = shell.map(tpgid::of);
        let by_ps = shell.and_then(|shell| {
            ps(&["-o", "tpgid=", "-p", &shell.to_string()])
                .and_then(|out| out.trim().parse::<u32>().ok())
        });
        // End what the test started, by the pids it knows: the shell's group, then `script`.
        if let Some(shell) = shell.and_then(|shell| libc::pid_t::try_from(shell).ok()) {
            // SAFETY: a signal to the group of the shell this test started and no other.
            unsafe { libc::kill(-shell, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        let Some(shell) = shell else {
            eprintln!("skipped: the shell on the terminal did not start");
            return;
        };
        assert_eq!(by_system, Some(tpgid::Tpgid::Group(Some(shell))));
        assert_eq!(by_ps, Some(shell), "ps disagrees with the system read");
    }

    #[test]
    fn the_command_line_of_a_live_process_is_read_and_a_dead_one_is_not() {
        let mine = command_line_of(std::process::id()).expect("ps names this process");
        assert!(mine.contains("taw"), "{mine}");
        // Not one line cut short: `ps` truncates to the terminal's width unless told not to.
        assert!(!mine.contains('\n'), "{mine}");
        assert_eq!(command_line_of(u32::MAX - 1), None);
    }
}
