//! When the watcher takes a pane's profile mark off (`@agent_sandbox`, with the options that go
//! with it, `store::SANDBOX_OPTIONS`).
//!
//! `taw sandbox` sets the mark in its own pane before it becomes the profile, with its own pid
//! (`@agent_sandbox_pid`), which `exec` keeps through the profile and the runtime; a restore sets
//! it with the line it types, and no pid. It outlives the agent's record, which the watcher clears
//! while a first start builds an image (the profile in front, a shell script, reads as the agent's
//! shell come back), and it has two ends: the agent leaves the pane, or the profile ended without
//! starting it.
//!
//! A mark with a process ends with it: the process that set the mark has ended with no container in
//! front. That is the profile ending without starting one, at once or after a wait, with a word or
//! without; and since the profile becomes the runtime, it is the container leaving the pane too.
//! While the process runs the mark stays, though a read cannot see the container for a moment (a
//! `ps` that failed under load): the container has not left a pane whose runtime still runs. A
//! mark with no process, a restore's, is kept until the line the restore typed runs and its `taw
//! sandbox` marks the pane again with its own pid, or until a container that came leaves: a
//! snapshot taken before then must still say the agent is a sandboxed one.
//!
//! A marked pane whose process still runs with no container in front is read again a second after
//! each read, whether or not it prints, so that a profile that ends without a word is seen ending.
//! A pid the system has given to another process by the time a read asks keeps the mark, as one
//! alive does, and so does a process this user may not signal (a profile that became `sudo ...`):
//! the safe side, never a sandboxed agent taken for a plain one. Only the system's word that
//! there is no such process ends it (`watch::process_alive`).

use std::time::{Duration, Instant};

/// How long after a read a marked pane whose profile still runs is read again, output or not.
pub const RECHECK: Duration = Duration::from_secs(1);

/// What one read of the pane found, for the mark.
#[derive(Clone, Copy, Debug)]
pub struct Read<'a> {
    /// `@agent_sandbox`, as the read found it.
    pub mark: Option<&'a str>,
    /// `@agent_sandbox_pid`, as the read found it: the process that set the mark.
    pub pid: Option<u32>,
    /// The runtime the pane was marked with (`@agent_container`) before this read, and after it.
    pub container_before: Option<&'a str>,
    pub container_after: Option<&'a str>,
    /// Whether the process that set the mark is alive, when this read asked ([`asks`]); `None`
    /// for a mark with no process, and while a container is in front.
    pub started_alive: Option<bool>,
}

/// What a read does with the mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Keep,
    /// Take it off, every option of it, and empty the pane's report folder.
    Clear,
}

/// Whether a read asks if the mark's process is alive: the pane is marked, the mark has a process,
/// and no container is in front, which says by itself that the agent is there.
pub fn asks(mark: Option<&str>, pid: Option<u32>, container_after: Option<&str>) -> bool {
    mark.is_some() && pid.is_some() && container_after.is_none()
}

/// What the watcher keeps of the mark between reads.
#[derive(Debug, Default)]
pub struct Mark {
    /// Whether the last read found the mark's process alive, with no container in front.
    waiting: bool,
    /// The mark the reads below are about, and its process: another one is another mark.
    of: Option<(String, Option<u32>)>,
    /// Whether a container has been in front of the pane while it held that mark: what a mark with
    /// no process ends with, once it leaves.
    came: bool,
}

impl Mark {
    /// The step for one read. A step to take the mark off that the pane did not take (it held
    /// another by then) is decided again at the next read, from what that read finds.
    pub fn step(&mut self, read: Read<'_>) -> Step {
        self.waiting = false;
        let Some(mark) = read.mark else {
            self.of = None;
            self.came = false;
            return Step::Keep;
        };
        if self.of.as_ref().map(|(held, pid)| (held.as_str(), *pid)) != Some((mark, read.pid)) {
            self.of = Some((mark.to_string(), read.pid));
            self.came = false;
        }
        self.came |= read.container_before.is_some();
        if read.container_after.is_some() {
            self.came = true;
            return Step::Keep;
        }
        match read.started_alive {
            Some(false) => Step::Clear,
            Some(true) => {
                self.waiting = true;
                Step::Keep
            }
            None if self.came => Step::Clear,
            None => Step::Keep,
        }
    }

    /// The pane did not take the step to take the mark off that the last read decided (a report
    /// landed between the read and the write): the next read decides again, and is owed
    /// [`RECHECK`] after it whether or not the pane prints, as a pane whose profile has ended may
    /// print nothing more.
    pub fn held_back(&mut self) {
        self.waiting = true;
    }

    /// The read owed whether or not the pane prints: [`RECHECK`] after the last one, while that
    /// found the mark's process alive with no container in front, or held back the mark's end.
    pub fn recheck(&self, last_read: Option<Instant>) -> Option<Instant> {
        if self.waiting {
            last_read.map(|last| last + RECHECK)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read<'a>(
        mark: Option<&'a str>,
        before: Option<&'a str>,
        after: Option<&'a str>,
        alive: Option<bool>,
    ) -> Read<'a> {
        Read {
            mark,
            pid: None,
            container_before: before,
            container_after: after,
            started_alive: alive,
        }
    }

    /// A read of a pane `taw sandbox` marked with its pid, 4242: whether that process is alive is
    /// asked only while no container is in front ([`asks`]).
    fn of_a_start<'a>(
        before: Option<&'a str>,
        after: Option<&'a str>,
        alive: Option<bool>,
    ) -> Read<'a> {
        Read {
            pid: Some(4242),
            ..read(Some("default"), before, after, alive)
        }
    }

    #[test]
    fn the_mark_goes_when_the_container_leaves_the_pane() {
        let mut mark = Mark::default();
        assert_eq!(
            mark.step(read(Some("default"), None, Some("docker"), None)),
            Step::Keep
        );
        assert_eq!(
            mark.step(read(Some("default"), Some("docker"), Some("docker"), None)),
            Step::Keep
        );
        assert_eq!(
            mark.step(read(Some("default"), Some("docker"), None, None)),
            Step::Clear
        );
    }

    /// The profile becomes the runtime, so its process ends with the container: the read that
    /// finds the container gone finds the process gone too, and takes the mark off.
    #[test]
    fn the_mark_of_a_profile_goes_with_its_process_when_the_container_leaves() {
        let mut mark = Mark::default();
        assert_eq!(
            mark.step(of_a_start(None, Some("docker"), None)),
            Step::Keep
        );
        assert_eq!(
            mark.step(of_a_start(Some("docker"), None, Some(false))),
            Step::Clear
        );
    }

    /// A read that cannot see the container for a moment (a `ps` that failed under load) while the
    /// process that set the mark, the runtime itself, runs on: the container has not left, and the
    /// mark stays, read again a second later.
    #[test]
    fn a_container_not_seen_for_a_moment_while_its_runtime_runs_keeps_the_mark() {
        let mut mark = Mark::default();
        mark.step(of_a_start(None, Some("docker"), None));
        let last = Instant::now();

        assert_eq!(
            mark.step(of_a_start(Some("docker"), None, Some(true))),
            Step::Keep
        );
        assert_eq!(mark.recheck(Some(last)), Some(last + RECHECK));
        assert_eq!(
            mark.step(of_a_start(None, Some("docker"), None)),
            Step::Keep
        );
        assert_eq!(
            mark.step(of_a_start(Some("docker"), None, Some(false))),
            Step::Clear,
            "and goes once the runtime has ended"
        );
    }

    #[test]
    fn a_profile_whose_process_has_ended_with_no_container_takes_its_mark_with_it() {
        let mut mark = Mark::default();
        assert_eq!(
            mark.step(read(Some("default"), None, None, Some(true))),
            Step::Keep
        );
        assert_eq!(
            mark.step(read(Some("default"), None, None, Some(false))),
            Step::Clear
        );
    }

    /// A profile that fails at once is gone at the first read that looks.
    #[test]
    fn a_profile_that_ended_before_any_read_looked_still_takes_its_mark_with_it() {
        let mut mark = Mark::default();
        assert_eq!(
            mark.step(read(Some("default"), None, None, Some(false))),
            Step::Clear
        );
    }

    /// A restore marks the pane with the line it types, and no process: the mark is kept until that
    /// line's `taw sandbox` marks the pane again, with its own.
    #[test]
    fn a_mark_with_no_process_is_kept() {
        let mut mark = Mark::default();
        for _ in 0..5 {
            assert_eq!(
                mark.step(read(Some("default"), None, None, None)),
                Step::Keep
            );
        }
    }

    /// A mark with no process whose container came and left is taken off, and decided so again at
    /// the next read while the pane still holds that same mark: a step the pane did not take (a
    /// report landed between the read and the write) is not forgotten. Another mark, or none, is
    /// another story.
    #[test]
    fn a_mark_with_no_process_whose_container_left_is_taken_off_until_it_goes() {
        let mut mark = Mark::default();
        mark.step(read(Some("default"), None, Some("docker"), None));
        assert_eq!(
            mark.step(read(Some("default"), Some("docker"), None, None)),
            Step::Clear
        );
        assert_eq!(
            mark.step(read(Some("default"), None, None, None)),
            Step::Clear,
            "the same mark, still there at the next read"
        );

        assert_eq!(
            mark.step(of_a_start(None, None, Some(true))),
            Step::Keep,
            "taw sandbox marked it again, with its own process"
        );
        assert_eq!(
            mark.step(read(Some("default"), None, None, None)),
            Step::Keep,
            "a restore's mark of its own, after that one"
        );
        mark.step(read(None, None, None, None));
        assert_eq!(
            mark.step(read(Some("default"), None, None, None)),
            Step::Keep
        );
    }

    /// A watcher started again finds the pane marked with a runtime that has gone since its
    /// predecessor last read it: the first step of a restore's mark, which has no process, sees the
    /// container leave all the same, and takes the mark off.
    #[test]
    fn a_mark_with_no_process_whose_container_went_before_the_first_read_goes() {
        let mut mark = Mark::default();
        assert_eq!(
            mark.step(read(Some("default"), Some("docker"), None, None)),
            Step::Clear
        );
    }

    /// A step to take the mark off that the pane did not take (a report landed between the read and
    /// the write) is owed the next read a second later, whether or not the pane prints.
    #[test]
    fn a_mark_end_held_back_is_read_again_a_second_after() {
        let mut mark = Mark::default();
        let last = Instant::now();
        assert_eq!(mark.step(of_a_start(None, None, Some(false))), Step::Clear);
        assert_eq!(mark.recheck(Some(last)), None);
        mark.held_back();
        assert_eq!(mark.recheck(Some(last)), Some(last + RECHECK));
        assert_eq!(mark.step(of_a_start(None, None, Some(false))), Step::Clear);
        assert_eq!(
            mark.recheck(Some(last)),
            None,
            "only the read that was held back"
        );
    }

    #[test]
    fn a_running_profile_with_no_container_is_read_again_a_second_after_and_nothing_else_is() {
        let mut mark = Mark::default();
        let last = Instant::now();
        mark.step(read(Some("default"), None, None, Some(true)));
        assert_eq!(mark.recheck(Some(last)), Some(last + RECHECK));
        mark.step(read(Some("default"), None, Some("docker"), None));
        assert_eq!(
            mark.recheck(Some(last)),
            None,
            "an agent's pane is read on an agent's grid"
        );
        mark.step(read(Some("default"), None, None, None));
        assert_eq!(
            mark.recheck(Some(last)),
            None,
            "a mark with no process waits for output"
        );
        mark.step(read(None, None, None, None));
        assert_eq!(mark.recheck(Some(last)), None);
    }

    #[test]
    fn the_process_is_asked_about_only_for_a_marked_pane_with_one_and_no_container() {
        assert!(asks(Some("default"), Some(4242), None));
        assert!(!asks(Some("default"), None, None));
        assert!(!asks(Some("default"), Some(4242), Some("docker")));
        assert!(!asks(None, Some(4242), None));
    }
}
