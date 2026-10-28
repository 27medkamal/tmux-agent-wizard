//! The job in front of a pane, read off the process table for [`crate::detect::job`].
//!
//! The terminal's foreground process group is what tmux names a pane's command by (its leader's name),
//! so that group is asked of the pane's own shell (`ps -o tpgid=`), its members of the process table
//! (`pgrep -g`, [`members_of_group`]), their names of one `ps`, and their arguments of each process:
//! exactly where the system gives them ([`container::exact_args_of`]) and as the line `ps` prints for them where it does not. Arguments can
//! carry credentials (`-e TOKEN=...`), so nothing here logs or keeps them: what comes out is read by
//! [`crate::detect::job::identify`] and dropped. The one exception is a Claude's flags that set how its
//! session runs (`--effort`, `--model`, `--permission-mode`, `--agent` with their values, and
//! `--dangerously-skip-permissions`), which `taw pane report-agent-session` keeps with the session so that
//! a restore can type them back ([`crate::claude_flags::kept`]): it reads them from the arguments of
//! the process [`crate::detect::job::claude_process`] names, drops everything else unread, and logs
//! nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::state::is_shell;
use super::{container, printed_by, printed_whatever_the_status, split_field};
use crate::detect;
use crate::detect::job::{Job, Process};

/// The least time between two askings of which process is in front of a pane whose command is a shell
/// or a runtime while its job could not be read: a `ps` call, as for a container runtime.
const LEADER_CHECK: Duration = Duration::from_secs(1);

/// How long after a leader comes in front a job that named no agent is read again on the recheck: a
/// wrapper may start its agent a moment after it starts itself. After that the leader's "no agent"
/// stands, so a long-running dev server or build costs no more job reads. A leader that has named an
/// agent once is outside this window: it is read at every recheck for as long as it leads.
const NAMING_WINDOW: Duration = Duration::from_secs(60);

/// What the watcher of one pane knows about the job in front of it, when tmux names the pane's command
/// by a shell or a runtime ([`detect::job::is_generic_runtime_or_shell`]): `node` for an npm Codex,
/// `sh` for a script, which name no agent themselves. The job is read for each leader of the
/// terminal's foreground group, and named by [`detect::job::identify`]: once when it named an agent,
/// and on the recheck during the leader's first [`NAMING_WINDOW`] in front when it named none (and
/// for as long as it leads, once it has named one). What
/// is kept is the agent's id, never the arguments.
#[derive(Debug)]
pub struct Probe {
    /// How long a job that was read, or a shell at its prompt, is left before the leader is asked
    /// again under the same command: the watcher's identity recheck.
    recheck: Duration,
    last: Option<Last>,
    /// What the last read of the job answered (`None` before the first read, `Some(None)` for no
    /// agent), kept across leaders: the watcher logs an answer only when it differs from this.
    last_answer: Option<Option<String>>,
    /// Whether the last look read the job and its answer differs from the read before it: what the
    /// watcher logs, by the agent's name alone.
    answer_changed: bool,
}

/// What the last look at the job in front of the pane, under `command`, found.
#[derive(Debug)]
struct Last {
    command: String,
    /// The leader of the foreground group, `None` when it could not be found.
    leader: Option<u32>,
    stand: Stand,
    /// When the leader was last asked.
    asked: Instant,
    /// The process the job was named by, while it names an agent: confirmed at every look, alive and
    /// still in the leader's group, so the name goes when that process does.
    named_by: Option<u32>,
    /// When this leader was first seen in front under this command.
    since: Instant,
    /// Whether the job of this leader has named an agent at some look: such a leader is read again at
    /// every recheck for as long as it leads, since a wrapper that restarts its agent
    /// (`while :; do kimi; done`) names a new one whenever it comes back.
    named_once: bool,
}

/// Where the reading of the job stands.
#[derive(Debug, Clone, PartialEq)]
enum Stand {
    /// The leader is the pane's own shell, at its prompt: no job was read.
    AtPrompt,
    /// The job was read, and names this.
    Read(Option<String>),
    /// The leader, or the job, could not be read, so many times in a row: tried again at the next
    /// look, and after [`container::ATTEMPTS`] taken as naming no agent until the recheck.
    Failed(u8),
}

impl Probe {
    /// A probe that asks again after `recheck` about a job that named no agent.
    pub fn new(recheck: Duration) -> Probe {
        Probe {
            recheck,
            last: None,
            last_answer: None,
            answer_changed: false,
        }
    }

    /// The agent the job in front of the pane is, when its foreground `command` is a shell or a
    /// runtime, at `now`. `front` gives the leader of the terminal's foreground group and the pane's
    /// own shell; `read` the job that leader leads. Neither is asked for any other command; the leader
    /// once a recheck, or at most once a second ([`LEADER_CHECK`]) while its job could not be read.
    /// The job is not read while the pane's own shell leads at its prompt (the leader is the shell and
    /// the command a shell); it is read once for each leader that named an agent, and on the recheck
    /// during a leader's first [`NAMING_WINDOW`] in front while it named none, or for as long as it
    /// leads once it has named an agent: a wrapper restarts its agent.
    ///
    /// A job that named an agent is confirmed at every look by `still` (whether the process it was
    /// named by is alive and in the leader's group, which asks no other program): the pane's own shell
    /// and the agent's script can both be `bash` to tmux, so its exit need not change the command, and
    /// the name goes at the first look after that process has gone. A leader that cannot be asked
    /// leaves a confirmed name as it is: it says nothing of the job.
    pub fn agent(
        &mut self,
        command: &str,
        now: Instant,
        front: impl FnOnce() -> Option<(u32, u32)>,
        read: impl FnOnce(u32) -> Option<Job>,
        mut still: impl FnMut(u32, u32) -> bool,
    ) -> Option<String> {
        self.answer_changed = false;
        if !detect::job::is_generic_runtime_or_shell(&command.to_lowercase()) {
            self.last = None;
            return None;
        }
        let confirmed =
            self.last
                .as_ref()
                .is_none_or(|last| match (&last.stand, last.leader, last.named_by) {
                    (Stand::Read(Some(_)), Some(leader), Some(pid)) => still(pid, leader),
                    _ => true,
                });
        if let Some(last) = self
            .last
            .as_ref()
            .filter(|last| confirmed && last.command == command)
        {
            let wait = match last.stand {
                Stand::AtPrompt | Stand::Read(_) => self.recheck,
                Stand::Failed(_) => LEADER_CHECK,
            };
            if now.saturating_duration_since(last.asked) < wait {
                return Self::named(&last.stand);
            }
        }
        let front = front();
        let leader = front.map(|(leader, _)| leader);
        // A leader that could not be asked is no other leader: what was confirmed of the job stands.
        let before = self
            .last
            .take()
            .filter(|last| last.command == command && (last.leader == leader || front.is_none()));
        let since = before.as_ref().map_or(now, |last| last.since);
        let named_before = before.as_ref().is_some_and(|last| last.named_once);
        let (kept_leader, kept_by) = before
            .as_ref()
            .map_or((None, None), |last| (last.leader, last.named_by));
        let before = before.map(|last| last.stand);
        let mut named_by = None;
        let settled = now.saturating_duration_since(since) >= NAMING_WINDOW;
        let failed = |before: Option<Stand>| match before {
            Some(Stand::Failed(tries)) if tries + 1 >= container::ATTEMPTS => Stand::Read(None),
            Some(Stand::Failed(tries)) => Stand::Failed(tries + 1),
            _ => Stand::Failed(1),
        };
        let stand = match (front, before) {
            (None, Some(named @ Stand::Read(Some(_)))) if confirmed => {
                named_by = kept_by;
                named
            }
            (None, before) => failed(before),
            // The pane's own process leads: a shell at its prompt, or a runtime that is the pane's
            // process (`exec codex`, a window started with `codex`), which is read like any other.
            (Some((leader, shell)), _) if leader == shell && is_shell(command) => Stand::AtPrompt,
            (Some(_), Some(named @ Stand::Read(Some(_)))) if confirmed => {
                named_by = kept_by;
                named
            }
            (Some(_), Some(Stand::Read(None))) if settled && !named_before => Stand::Read(None),
            (Some((leader, _)), before) => match read(leader) {
                Some(job) => {
                    // A container runtime in the job names nothing here: the container look names
                    // one in front of the pane, with its marker, and a shell kept in front of one
                    // names no agent.
                    let named = detect::job::identify_process_outside_containers(&job);
                    named_by = named.as_ref().map(|(_, pid)| *pid);
                    let answer = named.map(|(agent, _)| agent);
                    let answer_before = self.last_answer.replace(answer.clone());
                    self.answer_changed = answer_before.as_ref() != Some(&answer);
                    Stand::Read(answer)
                }
                None => failed(before),
            },
        };
        let agent = Self::named(&stand);
        let leader = if front.is_none() && named_by.is_some() {
            kept_leader
        } else {
            leader
        };
        self.last = Some(Last {
            command: command.to_string(),
            leader,
            stand,
            asked: now,
            named_by,
            since,
            named_once: named_before || named_by.is_some(),
        });
        agent
    }

    fn named(stand: &Stand) -> Option<String> {
        match stand {
            Stand::Read(agent) => agent.clone(),
            Stand::AtPrompt | Stand::Failed(_) => None,
        }
    }

    /// Whether the last look read the job in front of the pane and its answer (the agent, or none)
    /// differs from the read before it in this pane; the first read of a pane differs. A leader that
    /// is read at every recheck and gives the same answer each time is told once.
    pub fn answer_changed(&self) -> bool {
        self.answer_changed
    }

    /// Forgets that the pane's own shell was at its prompt, so that the next look asks for the leader
    /// again: the watcher's, while a hook-owned record stands, whose end that answer decides. An agent
    /// a script runs under the shell's own name (`bash ~/bin/kimi-wrap` in a bash pane) leaves the
    /// command as it was, and a prompt seen before it started would end its record.
    pub fn forget_prompt(&mut self) {
        if self
            .last
            .as_ref()
            .is_some_and(|last| last.stand == Stand::AtPrompt)
        {
            self.last = None;
        }
    }

    /// Whether the job in front could not be read at the last look: a pane that may be running an
    /// agent, and is looked at again although nothing may print.
    pub fn wants_recheck(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|last| matches!(last.stand, Stand::Failed(_)))
    }
}

/// The processes in the foreground of the terminal of the pane whose own process is `pane_pid`, with
/// their names and arguments. `None` when the pane has no terminal, or the process table will not say
/// who is in front.
pub(crate) fn foreground_job(pane_pid: u32) -> Option<Job> {
    job_of_group(container::foreground_of(pane_pid)??)
}

/// Whether the process `pid` is alive and in the process group `group`, asked of the kernel and of no
/// other program: what confirms the process a job was named by. A group the kernel will not give
/// (`EPERM`) is not held against a live process.
pub(crate) fn still_in_group(pid: u32, group: u32) -> bool {
    let Ok(id) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: `getpgid` takes an integer and touches no memory of this process.
    let found = unsafe { libc::getpgid(id) };
    if found >= 0 {
        return u32::try_from(found).ok() == Some(group);
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) && super::process_alive(pid)
}

/// The processes of the process group `group`, with their names and arguments, `None` when it has
/// none or the process table will not say.
pub(crate) fn job_of_group(group: u32) -> Option<Job> {
    let pids = members_of_group(group)?;
    let names = names_of(&listing_of(&pids)?);
    let processes: Vec<Process> = pids
        .iter()
        .filter_map(|&pid| {
            let name = names.get(&pid)?.clone();
            let argv = container::exact_args_of(pid);
            let line = if argv.is_none() {
                container::command_line_of(pid)
            } else {
                None
            };
            Some(Process {
                pid,
                name,
                argv,
                line,
            })
        })
        .collect();
    (!processes.is_empty()).then_some(Job { group, processes })
}

/// The processes in the process group `group`, as `pgrep -g <group>` lists them (with `-a` on macOS),
/// `None` when there is none or `pgrep` cannot say.
///
/// macOS gets the `-a`. There `pgrep` otherwise leaves out every process it descends from (and with
/// `-a` it lists them, though never itself). `taw pane report-agent-session` runs as a descendant of
/// the Claude whose hook it is (`claude`, a shell, `python3`, `taw`, `pgrep`), so without the `-a`
/// the group's leader, the one process whose arguments are wanted, is not listed, and the pane's flags
/// cannot be kept. Everywhere else (procps) plain `pgrep -g` already lists the whole group, the
/// ancestors of the asker among them, and `-a` would only add each member's command line, which
/// [`members_in`] does not read, and which an older procps does not take. Either way `pgrep` never
/// lists itself. [`super::pane_shell_is_idle`] reads its group the same way, for the same reason.
/// Reading the whole process table with `ps -A` instead gives the same list for several times the
/// processor time (about 53 ms a call against about 9 ms, on a table of 2300 processes).
pub(super) fn members_of_group(group: u32) -> Option<Vec<u32>> {
    let group = group.to_string();
    let args: &[&str] = if cfg!(target_os = "macos") {
        &["-a", "-g", &group]
    } else {
        &["-g", &group]
    };
    let listing = printed_by("pgrep", args)?;
    let members = members_in(&listing);
    (!members.is_empty()).then_some(members)
}

/// The pids in what `pgrep -g` printed: the first field of each line, which is followed by the command
/// line when `-a` was given (macOS). A line that does not begin with a pid names
/// nothing.
fn members_in(listing: &str) -> Vec<u32> {
    listing
        .lines()
        .filter_map(|line| split_field(line)?.0.parse().ok())
        .collect()
}

/// What `ps` prints of `pids`: a line of the pid and the program for each that is there. Read whatever
/// `ps` exits with, since one of them having gone since `pgrep` listed it is an error to some `ps`es,
/// and the lines of the others are there all the same.
fn listing_of(pids: &[u32]) -> Option<String> {
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    printed_whatever_the_status("ps", &["-ww", "-o", "pid=,comm=", "-p", &list])
}

/// The name of each process in what `ps -o pid=,comm=` printed: the last component of the program, which
/// macOS prints as a path (`/bin/sh`) and Linux as a name that may have spaces in it.
fn names_of(listing: &str) -> HashMap<u32, String> {
    let mut names = HashMap::new();
    for line in listing.lines() {
        let Some((pid, program)) = split_field(line) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let name = program.trim().rsplit('/').next().unwrap_or_default().trim();
        if !name.is_empty() {
            names.insert(pid, name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECHECK: Duration = Duration::from_secs(5);

    fn process(pid: u32, name: &str, argv: &[&str]) -> Process {
        Process {
            pid,
            name: name.to_string(),
            argv: Some(argv.iter().map(|arg| arg.to_string()).collect()),
            line: None,
        }
    }

    /// The job of an npm Codex as the process table had it on 2026-10-26: `node .../bin/codex`
    /// leading, the native binary its child.
    fn npm_codex(group: u32) -> Job {
        Job {
            group,
            processes: vec![
                process(
                    group,
                    "node",
                    &[
                        "node",
                        "/Users/u/.local/share/mise/installs/npm-openai-codex/0.164.0/bin/codex",
                    ],
                ),
                process(
                    group + 1,
                    "codex",
                    &["/x/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"],
                ),
            ],
        }
    }

    /// One look of `probe` at `command`, with the leader and shell `front`, counting the job reads in
    /// `reads`.
    fn look(
        probe: &mut Probe,
        command: &str,
        now: Instant,
        front: Option<(u32, u32)>,
        job: Option<Job>,
        reads: &mut u32,
    ) -> Option<String> {
        probe.agent(
            command,
            now,
            || front,
            |_| {
                *reads += 1;
                job
            },
            |_, _| true,
        )
    }

    #[test]
    fn a_node_job_that_runs_codex_is_codex_and_is_read_once_for_its_leader() {
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let front = Some((500, 100));

        let agent = look(
            &mut probe,
            "node",
            start,
            front,
            Some(npm_codex(500)),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("codex"));
        assert_eq!(reads, 1);

        // The same leader, a minute on: still Codex, and not read again.
        for second in 1..60 {
            let at = start + Duration::from_secs(second);
            let agent = look(&mut probe, "node", at, front, None, &mut reads);
            assert_eq!(agent.as_deref(), Some("codex"));
        }
        assert_eq!(reads, 1);
        assert!(!probe.wants_recheck());

        // Another leader under the same command is another job, read again.
        let at = start + Duration::from_secs(61);
        let agent = look(
            &mut probe,
            "node",
            at,
            Some((700, 100)),
            Some(Job::default()),
            &mut reads,
        );
        assert_eq!(agent, None);
        assert_eq!(reads, 2);
    }

    #[test]
    fn a_script_run_by_sh_is_the_agent_its_path_names() {
        let mut probe = Probe::new(RECHECK);
        let mut reads = 0;
        let job = Job {
            group: 300,
            processes: vec![
                process(300, "sh", &["sh", "/tmp/fake/codex"]),
                process(301, "sleep", &["sleep", "0.2"]),
            ],
        };

        let agent = look(
            &mut probe,
            "sh",
            Instant::now(),
            Some((300, 100)),
            Some(job),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("codex"));
    }

    #[test]
    fn a_container_runtime_a_shell_keeps_behind_it_names_no_agent() {
        // A sandbox profile that runs `docker run ... claude` without `exec`: its shell leads the job
        // in front of the pane and the runtime is its child. The runtime's arguments name Claude, but
        // the shell in front gets no agent: the container look names a runtime only when it is in
        // front, where it marks the pane with it as well.
        let mut probe = Probe::new(RECHECK);
        let mut reads = 0;
        let job = Job {
            group: 300,
            processes: vec![
                process(300, "sh", &["sh", "/profiles/box", "claude"]),
                process(
                    301,
                    "docker",
                    &["docker", "run", "--rm", "-it", "box-image", "claude"],
                ),
            ],
        };
        assert_eq!(
            detect::job::identify(&job).as_deref(),
            Some("claude"),
            "the verbs still see the runtime's agent in the job"
        );

        let agent = look(
            &mut probe,
            "sh",
            Instant::now(),
            Some((300, 100)),
            Some(job),
            &mut reads,
        );
        assert_eq!(reads, 1);
        assert_eq!(agent, None);
    }

    #[test]
    fn a_shell_at_its_prompt_reads_no_job() {
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let mut asked = 0;

        for second in 0..30 {
            let at = start + Duration::from_secs(second);
            let agent = probe.agent(
                "zsh",
                at,
                || {
                    asked += 1;
                    Some((100, 100))
                },
                |_| {
                    reads += 1;
                    Some(npm_codex(100))
                },
                |_, _| true,
            );
            assert_eq!(agent, None);
        }
        assert_eq!(reads, 0, "a shell at its prompt had its job read");
        // Its leader is asked once a recheck, not once a second.
        assert_eq!(asked, 6);
        assert!(!probe.wants_recheck());
    }

    #[test]
    fn a_runtime_that_is_the_panes_own_process_is_read_for_its_job() {
        // `exec codex` at the prompt, or a window started with `codex`: node is the pane's own
        // process, so it leads the foreground group, and it is no shell at its prompt.
        let mut probe = Probe::new(RECHECK);
        let mut reads = 0;
        let agent = look(
            &mut probe,
            "node",
            Instant::now(),
            Some((100, 100)),
            Some(npm_codex(100)),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("codex"));
        assert_eq!(reads, 1);
    }

    #[test]
    fn a_job_that_named_no_agent_is_read_again_only_in_its_leaders_first_minute() {
        // `npm run dev`: a wrapper may start its agent a moment after it starts, so the job is read
        // again on the recheck while the leader is new, and after that its "no agent" stands.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let mut read_at = Vec::new();
        for second in 0..=180 {
            let at = start + Duration::from_secs(second);
            let before = reads;
            let agent = look(
                &mut probe,
                "node",
                at,
                Some((500, 100)),
                Some(Job::default()),
                &mut reads,
            );
            assert_eq!(agent, None);
            if reads > before {
                read_at.push(second);
            }
        }
        let inside: Vec<u64> = (0..NAMING_WINDOW.as_secs())
            .step_by(RECHECK.as_secs() as usize)
            .collect();
        assert_eq!(
            read_at, inside,
            "read inside the window, and never after it"
        );

        // Another leader is new again, and is read.
        let at = start + Duration::from_secs(190);
        let agent = look(
            &mut probe,
            "node",
            at,
            Some((700, 100)),
            Some(npm_codex(700)),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("codex"));
    }

    #[test]
    fn a_wrapper_that_restarts_its_agent_has_the_new_one_named_at_the_next_recheck() {
        // `while :; do codex; done`: the agent goes, a read in the gap names none, and it comes back
        // after the leader's first minute.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let wrapper = Job {
            group: 500,
            processes: vec![process(500, "node", &["node", "/tmp/wrap.js"])],
        };
        let mut named_at = Vec::new();
        for second in 0..=90u64 {
            let at = start + Duration::from_secs(second);
            let agent = probe.agent(
                "node",
                at,
                || Some((500, 100)),
                |_| {
                    Some(if !(1..65).contains(&second) {
                        npm_codex(500)
                    } else {
                        wrapper.clone()
                    })
                },
                |_, _| !(1..66).contains(&second),
            );
            if agent.is_some() {
                named_at.push(second);
            }
        }
        // Asked at 1 (the agent was found gone) and every 5 s on: the first ask after 65 is at 66.
        assert_eq!(
            named_at,
            [0].into_iter().chain(66..=90).collect::<Vec<u64>>(),
            "the restarted agent was not named at the next recheck"
        );
    }

    #[test]
    fn the_answer_of_a_read_is_told_only_when_it_differs_from_the_read_before() {
        // The restart sequence above: named at 0, none at 1 (found gone, read in the gap), the same
        // none at every read to 61, named again at 66 and not read after. Reads at 6, 11 and on
        // that answer the same are not told.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let wrapper = Job {
            group: 500,
            processes: vec![process(500, "node", &["node", "/tmp/wrap.js"])],
        };
        let (mut read_at, mut told_at) = (Vec::new(), Vec::new());
        for second in 0..=90u64 {
            let at = start + Duration::from_secs(second);
            probe.agent(
                "node",
                at,
                || Some((500, 100)),
                |_| {
                    read_at.push(second);
                    Some(if !(1..65).contains(&second) {
                        npm_codex(500)
                    } else {
                        wrapper.clone()
                    })
                },
                |_, _| !(1..66).contains(&second),
            );
            if probe.answer_changed() {
                told_at.push(second);
            }
        }
        assert!(
            read_at.len() > 5,
            "the job was read at {read_at:?}, so the same answer was given more than once"
        );
        assert_eq!(told_at, [0, 1, 66], "read at {read_at:?}");
    }

    #[test]
    fn a_leader_that_named_an_agent_is_asked_for_on_the_recheck() {
        // Its exit changes the pane's command, which is seen at once anyway.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut asked = 0;
        let mut reads = 0;
        for second in 0..30 {
            let at = start + Duration::from_secs(second);
            let agent = probe.agent(
                "node",
                at,
                || {
                    asked += 1;
                    Some((500, 100))
                },
                |_| {
                    reads += 1;
                    Some(npm_codex(500))
                },
                |_, _| true,
            );
            assert_eq!(agent.as_deref(), Some("codex"));
        }
        assert_eq!(reads, 1);
        assert_eq!(asked, 6);
    }

    #[test]
    fn a_command_that_is_no_shell_or_runtime_reads_nothing() {
        let mut probe = Probe::new(RECHECK);
        let mut reads = 0;
        for command in ["vim", "less", "codex", "claude", "docker"] {
            let agent = probe.agent(
                command,
                Instant::now(),
                || panic!("the leader was asked for {command}"),
                |_| {
                    reads += 1;
                    None
                },
                |_, _| true,
            );
            assert_eq!(agent, None);
        }
        assert_eq!(reads, 0);
    }

    #[test]
    fn a_job_that_could_not_be_read_is_tried_again_and_then_left_to_the_recheck() {
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        for second in 0..u64::from(container::ATTEMPTS) {
            let at = start + Duration::from_secs(second);
            assert_eq!(
                look(&mut probe, "node", at, Some((500, 100)), None, &mut reads),
                None
            );
        }
        assert_eq!(reads, u32::from(container::ATTEMPTS));
        assert!(!probe.wants_recheck());
        // Then the recheck reads it again, and finds Codex.
        let at = start + Duration::from_secs(u64::from(container::ATTEMPTS) - 1) + RECHECK;
        let agent = look(
            &mut probe,
            "node",
            at,
            Some((500, 100)),
            Some(npm_codex(500)),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("codex"));
    }

    /// A bash script that is the agent (`bash /path/to/claude`, the test fake) leading group 500 in
    /// a pane whose own shell, 100, is bash to tmux as well.
    fn bash_script_claude() -> Job {
        Job {
            group: 500,
            processes: vec![process(500, "bash", &["bash", "/tmp/fake/claude"])],
        }
    }

    #[test]
    fn an_agent_whose_process_has_gone_is_named_no_more_at_the_next_look() {
        // The pane's own shell and the agent's script are both `bash`: the agent's exit leaves the
        // command as it was, and a name kept to the recheck said the agent was there for 5 s more.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let job = Some(bash_script_claude());
        let named = probe.agent("bash", start, || Some((500, 100)), |_| job, |_, _| true);
        assert_eq!(named.as_deref(), Some("claude"));
        let mut confirmed = Vec::new();
        let at = start + Duration::from_millis(300);
        let still = probe.agent(
            "bash",
            at,
            || panic!("a confirmed name asked for the leader inside the recheck"),
            |_| None,
            |pid, group| {
                confirmed.push((pid, group));
                true
            },
        );
        assert_eq!(still.as_deref(), Some("claude"));
        assert_eq!(confirmed, vec![(500, 500)], "the agent was not confirmed");
        // The agent has gone; the shell is back in front.
        let at = start + Duration::from_millis(600);
        let gone = probe.agent(
            "bash",
            at,
            || Some((100, 100)),
            |_| {
                reads += 1;
                None
            },
            |_, _| false,
        );
        assert_eq!(gone, None, "the name outlived the agent's process");
        assert_eq!(reads, 0, "a shell at its prompt had its job read");
    }

    #[test]
    fn a_wrapper_that_outlives_its_agent_is_read_again_and_loses_the_name() {
        // `claude "$@"; read -p done`: the wrapper leads on, its agent gone.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let agent = look(
            &mut probe,
            "node",
            start,
            Some((500, 100)),
            Some(npm_codex(500)),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("codex"));
        let at = start + Duration::from_secs(1);
        let left = probe.agent(
            "node",
            at,
            || Some((500, 100)),
            |_| {
                reads += 1;
                Some(Job {
                    group: 500,
                    processes: vec![process(500, "node", &["node", "/tmp/wrap.js"])],
                })
            },
            |_, _| false,
        );
        assert_eq!(left, None, "the wrapper kept its agent's name");
        assert_eq!(reads, 2);
    }

    #[test]
    fn a_leader_that_cannot_be_asked_leaves_a_confirmed_name() {
        // `ps` cannot fork under load: that says nothing of the job, and a wrapped hook agent's
        // record must not end on it.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let agent = look(
            &mut probe,
            "bash",
            start,
            Some((500, 100)),
            Some(bash_script_claude()),
            &mut reads,
        );
        assert_eq!(agent.as_deref(), Some("claude"));
        for second in [5, 6, 7, 12] {
            let at = start + Duration::from_secs(second);
            let agent = probe.agent("bash", at, || None, |_| None, |_, _| true);
            assert_eq!(agent.as_deref(), Some("claude"), "at {second} s");
        }
        // And the name is still confirmed against the leader it was read under.
        let at = start + Duration::from_secs(13);
        let mut asked = Vec::new();
        let agent = probe.agent(
            "bash",
            at,
            || None,
            |_| None,
            |pid, group| {
                asked.push((pid, group));
                false
            },
        );
        assert_eq!(agent, None, "a dead agent was kept on a failed ask");
        assert_eq!(asked, vec![(500, 500)]);
    }

    #[test]
    fn a_prompt_forgotten_is_asked_about_at_the_next_look() {
        // A bash pane at its prompt, then `bash ~/bin/claude-wrap` typed: the command stays `bash`.
        let mut probe = Probe::new(RECHECK);
        let start = Instant::now();
        let mut reads = 0;
        let agent = look(
            &mut probe,
            "bash",
            start,
            Some((100, 100)),
            None,
            &mut reads,
        );
        assert_eq!(agent, None);
        let at = start + Duration::from_secs(1);
        probe.forget_prompt();
        let agent = look(
            &mut probe,
            "bash",
            at,
            Some((500, 100)),
            Some(bash_script_claude()),
            &mut reads,
        );
        assert_eq!(
            agent.as_deref(),
            Some("claude"),
            "the prompt seen before stood"
        );
        // A name is not forgotten with it.
        probe.forget_prompt();
        let at = start + Duration::from_secs(2);
        let agent = probe.agent(
            "bash",
            at,
            || panic!("a name was forgotten with the prompt"),
            |_| None,
            |_, _| true,
        );
        assert_eq!(agent.as_deref(), Some("claude"));
    }

    #[test]
    fn a_process_is_still_in_a_group_when_it_is_alive_and_in_it() {
        let group = unsafe { libc::getpgid(0) };
        assert!(group > 0);
        let group = group as u32;
        assert!(
            still_in_group(std::process::id(), group),
            "a live process in its own group"
        );
        assert!(
            !still_in_group(std::process::id(), group + 1),
            "a live process in another group"
        );
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait for true");
        assert!(
            !still_in_group(pid, group),
            "a process that has gone and been reaped"
        );
    }

    #[test]
    fn a_program_is_named_by_the_last_component_of_its_path() {
        let names = names_of(
            "  101 /bin/sh\n  102 -zsh\n  103 /Applications/Some App.app/Contents/MacOS/some app\n  104 claude\n",
        );

        assert_eq!(names[&101], "sh");
        assert_eq!(names[&102], "-zsh");
        assert_eq!(names[&103], "some app");
        assert_eq!(names[&104], "claude");
    }

    #[test]
    fn the_members_of_a_group_are_the_first_field_of_each_line_pgrep_prints() {
        // Without `-a` (not macOS) each line is a pid alone, with it (macOS) a command line follows.
        assert_eq!(members_in("200\n201\n  203\n"), [200, 201, 203]);
        assert_eq!(
            members_in("200 /bin/sh -c x\n201 claude --effort high\n203 python3 hook.py\n"),
            [200, 201, 203]
        );
        assert!(members_in("").is_empty());
        assert!(members_in("\nx 200\n-1 y\n").is_empty());
    }

    /// Where [`a_group_is_listed_whole_to_a_member_and_to_a_descendant_of_one`] has the probe write
    /// what it saw; the probe is that test run again, as a process in a group of its own making.
    const PROBE_OUT: &str = "TAW_MEMBERS_PROBE_OUT";

    /// Not a test of its own: the part of the one below that runs inside the group. It writes what
    /// [`members_of_group`] lists for its parent's group, the pid of its parent (the shell that leads
    /// it) first and its own pid after.
    #[test]
    fn members_probe_inside_the_group() {
        let Some(out) = std::env::var_os(PROBE_OUT) else {
            return;
        };
        let shell = std::os::unix::process::parent_id();
        let members = members_of_group(shell).unwrap_or_default();
        std::fs::write(out, format!("{shell} {} {members:?}", std::process::id())).unwrap();
    }

    /// On macOS `pgrep -g` alone leaves out every ancestor of `pgrep`, so asked from inside the group
    /// it lists the group short: neither the shell that leads it nor the probe that asked. Elsewhere
    /// plain `pgrep -g` lists the whole group, and the test holds all the same.
    #[test]
    fn a_group_is_listed_whole_to_a_member_and_to_a_descendant_of_one() {
        use std::os::unix::process::CommandExt;
        if std::env::var_os(PROBE_OUT).is_some() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("taw-members-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("answer");
        let exe = std::env::current_exe().unwrap();
        // `; :` keeps the shell from replacing itself with the probe, so the probe is its child, in the
        // group the shell leads.
        let line = format!(
            "'{}' --exact watch::job::tests::members_probe_inside_the_group --test-threads=1; :",
            exe.display()
        );
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &line])
            .env(PROBE_OUT, &out)
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let seen = std::fs::read_to_string(&out);
        let _ = std::fs::remove_dir_all(&dir);
        let seen = seen.unwrap_or_else(|e| panic!("the probe wrote nothing ({status:?}, {e})"));
        let (shell, rest) = seen.split_once(' ').unwrap();
        let (probe, listed) = rest.split_once(' ').unwrap();
        let listed: Vec<&str> = listed
            .trim_matches(|c| c == '[' || c == ']')
            .split(", ")
            .collect();
        assert!(
            listed.contains(&shell),
            "the leader {shell} is not in {listed:?}"
        );
        assert!(
            listed.contains(&probe),
            "the asker {probe} is not in {listed:?}"
        );
    }

    #[test]
    fn lines_that_are_not_a_pid_and_a_program_name_nothing() {
        let names = names_of("\n  x /bin/sh\n  105\n  106 /\n  107 /bin/cat\n");

        assert_eq!(names.len(), 1);
        assert_eq!(names[&107], "cat");
    }

    #[test]
    fn the_job_in_front_of_this_process_is_its_own_group_when_it_has_a_terminal() {
        // A test run has no terminal, or the one of whoever runs it: either way the answer is a job
        // that holds processes, or none, and never a job with nobody in it.
        let me = std::process::id();
        if let Some(job) = foreground_job(me) {
            assert!(!job.processes.is_empty());
            assert!(job.processes.iter().all(|process| !process.name.is_empty()));
        }
    }
}
