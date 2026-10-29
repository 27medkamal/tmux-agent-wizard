//! A far command run to its end within a bound: the one runner the app's short verbs on a machine share
//! (the open of a worktree, the preview of a pane).

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much of a process's standard output is kept: past it the output is read and thrown away, so a far side
/// that streams cannot grow the app's memory, and the run ends as [`Ran::Done`] with what came before.
pub const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// How much the reader thread takes from the pipe at a time.
const CHUNK: usize = 4096;

/// How a bounded run ended.
#[derive(Debug)]
pub enum Ran {
    /// The process ended by itself: how, and what it wrote to its standard output.
    Done {
        code: Option<i32>,
        signal: Option<i32>,
        stdout: Vec<u8>,
        /// The last bytes of its standard error when the run kept them ([`run_keeping_stderr`]), else empty.
        stderr: Vec<u8>,
    },
    /// The process could not be started, or could not be waited for.
    Failed(String),
    /// The bound passed first: the process was ended by its pid and reaped.
    TimedOut,
    /// `ended` said the caller must stop: the process was ended by its pid and reaped.
    Ended,
}

/// The way to end a run from another thread, and the home of its child while it runs: [`Ender::end`] kills and
/// reaps the child itself, so a holder that is dropped (the app, on its way out) does not depend on the run's
/// thread being scheduled in time. The child is only ever killed through its handle, which does nothing for a
/// child already reaped, so a pid is never signalled twice.
#[derive(Default)]
pub struct Ender {
    stop: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Ender {
    pub fn new() -> Arc<Ender> {
        Arc::new(Ender::default())
    }

    /// Asks the run to end at its next look (it ends its child by the pid and reaps it, then says `Ended`).
    pub fn set(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether the run was asked to end.
    pub fn is_set(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Ends the run now: the child, if it is still there, is killed and reaped before this returns.
    pub fn end(&self) {
        self.set();
        if let Some(mut child) = self.slot().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.child.lock().unwrap_or_else(|gone| gone.into_inner())
    }
}

/// Runs `command` with its standard input empty, its standard error dropped and its standard output read, and
/// waits for it for at most `bound`. Every `look` it asks `ended(ctx)` and then lets `meanwhile(ctx)` do its
/// work (the machines keep moving while an app waits). The process is ended by its pid, which is this
/// process's child, when the bound passes or `ended` says so: nothing else is touched.
///
/// The output is read on its own thread and sent as it is read: a pipe nobody reads would stop the child, and a
/// process the child left behind may keep the pipe open long after the reply, so the end of the pipe is not
/// waited for. At most [`MAX_OUTPUT`] bytes (and one chunk) are kept; the rest is drained and dropped. After the child ends, what it wrote is collected for no longer than half a second (two seconds
/// while what came does not end in a newline: a reader thread a loaded machine did not run has cut the line).
pub fn run<C>(
    ctx: &mut C,
    command: Command,
    bound: Duration,
    look: Duration,
    ended: impl Fn(&C) -> bool,
    meanwhile: impl Fn(&mut C),
) -> Ran {
    run_in(
        &Ender::default(),
        ctx,
        command,
        bound,
        look,
        ended,
        meanwhile,
    )
}

/// [`run`] with its child kept in `ender`, which can end it from another thread ([`Ender::end`], or
/// [`Ender::set`] for the next look). A reader thread that cannot be made ends and reaps the child and gives
/// [`Ran::Failed`].
pub fn run_in<C>(
    ender: &Ender,
    ctx: &mut C,
    command: Command,
    bound: Duration,
    look: Duration,
    ended: impl Fn(&C) -> bool,
    meanwhile: impl Fn(&mut C),
) -> Ran {
    run_keeping_stderr(ender, ctx, command, bound, look, 0, ended, meanwhile)
}

/// [`run_in`] that also keeps the last `keep_stderr` bytes of the child's standard error in [`Ran::Done`]
/// (nothing is kept, and the standard error is dropped, when it is 0). The pipe is drained on a thread of its own
/// and, as the output is, not waited for to its end.
#[allow(clippy::too_many_arguments)]
pub fn run_keeping_stderr<C>(
    ender: &Ender,
    ctx: &mut C,
    mut command: Command,
    bound: Duration,
    look: Duration,
    keep_stderr: usize,
    ended: impl Fn(&C) -> bool,
    meanwhile: impl Fn(&mut C),
) -> Ran {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if keep_stderr == 0 {
            Stdio::null()
        } else {
            Stdio::piped()
        });
    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (err_over, err_ended) = mpsc::channel::<()>();
    // The slot is held from before the spawn until the child is stored, so [`Ender::end`] either runs first (and
    // nothing is spawned) or waits for the child to be there and ends it. Nothing under the lock waits for the
    // child or for anything that needs the lock: a spawn returns once the child has exec'd, the reader thread
    // never takes the lock, and a failed start kills the child it just made before it lets go.
    let (sent, reply) = mpsc::channel::<Vec<u8>>();
    {
        let mut slot = ender.slot();
        if ender.stop.load(Ordering::SeqCst) {
            return Ran::Ended;
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => return Ran::Failed(err.to_string()),
        };
        if let Some(mut pipe) = child.stdout.take() {
            let reader = std::thread::Builder::new().spawn(move || {
                let mut chunk = [0u8; CHUNK];
                let mut kept = 0usize;
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            // Past the cap the pipe is still drained (a full pipe would stop the child), but nothing is sent.
                            if kept < MAX_OUTPUT {
                                kept += read;
                                if sent.send(chunk[..read].to_vec()).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            if let Err(err) = reader {
                // Nothing would read the pipe: the child is ended and reaped, and nothing runs.
                let _ = child.kill();
                let _ = child.wait();
                return Ran::Failed(err.to_string());
            }
        }
        if let Some(mut pipe) = child.stderr.take() {
            let tail = tail.clone();
            let spawned = std::thread::Builder::new().spawn(move || {
                let mut chunk = [0u8; CHUNK];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut kept = tail.lock().unwrap_or_else(|gone| gone.into_inner());
                            kept.extend_from_slice(&chunk[..read]);
                            if kept.len() > keep_stderr {
                                let cut = kept.len() - keep_stderr;
                                kept.drain(..cut);
                            }
                        }
                    }
                }
                let _ = err_over.send(());
            });
            if let Err(err) = spawned {
                let _ = child.kill();
                let _ = child.wait();
                return Ran::Failed(err.to_string());
            }
        }
        *slot = Some(child);
    }
    let until = Instant::now() + bound;
    let status = loop {
        let polled = match ender.slot().as_mut() {
            Some(child) => child.try_wait(),
            // Ended from outside (and reaped there).
            None => return Ran::Ended,
        };
        match polled {
            Ok(Some(status)) => {
                ender.slot().take();
                break status;
            }
            Ok(None) => {}
            Err(err) => {
                // As the timeout arm does: the child is ended and reaped before the wait is given up.
                ender.end();
                return Ran::Failed(err.to_string());
            }
        }
        let stop = ended(ctx) || ender.stop.load(Ordering::SeqCst);
        if Instant::now() >= until || stop {
            ender.end();
            return if stop { Ran::Ended } else { Ran::TimedOut };
        }
        meanwhile(ctx);
        std::thread::sleep(look);
    };
    let mut bytes = Vec::new();
    let began = Instant::now();
    loop {
        let limit = if bytes.ends_with(b"\n") {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(2)
        };
        let Some(left) = limit.checked_sub(began.elapsed()) else {
            break;
        };
        let Ok(chunk) = reply.recv_timeout(left) else {
            break;
        };
        bytes.extend(chunk);
    }
    // What the child said on its standard error: its end is waited for no longer than the output's, and what
    // came by then is what is kept.
    if keep_stderr > 0 {
        let _ = err_ended.recv_timeout(Duration::from_millis(500));
    }
    let stderr = tail.lock().unwrap_or_else(|gone| gone.into_inner()).clone();
    use std::os::unix::process::ExitStatusExt;
    Ran::Done {
        code: status.code(),
        signal: status.signal(),
        stdout: bytes,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn never(_: &()) -> bool {
        false
    }

    #[test]
    fn what_a_process_writes_comes_back_with_how_it_ended() {
        let ran = run(
            &mut (),
            sh("printf 'one\\ntwo\\n'; exit 3"),
            Duration::from_secs(20),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        let Ran::Done { code, stdout, .. } = ran else {
            panic!("expected Done, got {ran:?}");
        };
        assert_eq!(code, Some(3));
        assert_eq!(stdout, b"one\ntwo\n");
    }

    #[test]
    fn a_process_that_writes_more_than_the_cap_ends_promptly_with_the_bytes_up_to_it() {
        let began = Instant::now();
        let ran = run(
            &mut (),
            sh("head -c 6000000 /dev/zero"),
            Duration::from_secs(20),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        let Ran::Done { code, stdout, .. } = ran else {
            panic!("expected Done, got {ran:?}");
        };
        assert_eq!(code, Some(0));
        assert!(!stdout.is_empty(), "what came before the cap is kept");
        assert!(
            stdout.len() <= MAX_OUTPUT + CHUNK,
            "{} bytes kept, the cap is {MAX_OUTPUT}",
            stdout.len()
        );
        assert!(began.elapsed() < Duration::from_secs(15));
    }

    #[test]
    fn the_last_bytes_of_the_standard_error_are_kept_when_asked_for_and_only_then() {
        let script = "printf 'first line\\n' >&2; head -c 3000 /dev/zero | tr '\\0' x >&2; printf '\\nlast line\\n' >&2; exit 2";
        let ran = run_keeping_stderr(
            &Ender::default(),
            &mut (),
            sh(script),
            Duration::from_secs(20),
            Duration::from_millis(10),
            2048,
            never,
            |_| {},
        );
        let Ran::Done { code, stderr, .. } = ran else {
            panic!("expected Done, got {ran:?}");
        };
        assert_eq!(code, Some(2));
        assert_eq!(stderr.len(), 2048, "the tail, cut to the cap");
        assert!(
            stderr.ends_with(b"\nlast line\n"),
            "the end is what is kept"
        );
        assert!(!String::from_utf8_lossy(&stderr).contains("first line"));
        let ran = run(
            &mut (),
            sh(script),
            Duration::from_secs(20),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        let Ran::Done { stderr, .. } = ran else {
            panic!("expected Done, got {ran:?}");
        };
        assert!(stderr.is_empty(), "nothing is kept unless asked for");
    }

    #[test]
    fn a_process_that_outlives_the_bound_is_ended_and_the_wait_is_over() {
        let began = Instant::now();
        let ran = run(
            &mut (),
            sh("exec sleep 30"),
            Duration::from_millis(300),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        assert!(matches!(ran, Ran::TimedOut), "{ran:?}");
        assert!(began.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_caller_that_must_stop_ends_the_process_and_meanwhile_ran_while_it_waited() {
        let mut looks = 0u32;
        let ran = run(
            &mut looks,
            sh("exec sleep 30"),
            Duration::from_secs(20),
            Duration::from_millis(10),
            |looks| *looks >= 3,
            |looks| *looks += 1,
        );
        assert!(matches!(ran, Ran::Ended), "{ran:?}");
        assert_eq!(looks, 3);
    }

    #[test]
    fn a_command_that_cannot_start_is_a_failure_with_its_reason() {
        let ran = run(
            &mut (),
            Command::new("/nonexistent/taw-bounded-test"),
            Duration::from_secs(1),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        assert!(matches!(ran, Ran::Failed(_)), "{ran:?}");
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the pid exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn pid_in(pidfile: &std::path::Path) -> i32 {
        std::fs::read_to_string(pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Kills the process a test started if a failed assert left it running: only while armed, and only if the
    /// pid's command line still names the process the test started.
    struct KillPidIn {
        pidfile: std::path::PathBuf,
        far: &'static str,
        armed: bool,
    }

    impl Drop for KillPidIn {
        fn drop(&mut self) {
            let pid = pid_in(&self.pidfile);
            if !self.armed || pid <= 1 {
                return;
            }
            let command = Command::new("ps")
                .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            if command.contains(self.far) {
                // SAFETY: kill(2) takes two integers; the pid is the test's process, checked just now by its command.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    /// The run's thread looks every ten seconds, so only [`Ender::end`] itself can have ended the child when
    /// this asserts, at once. Mutation: `end()` only sets the flag (as it did before) and this fails.
    #[test]
    fn end_kills_and_reaps_the_child_itself_without_the_runs_look() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut guard = KillPidIn {
            pidfile: pidfile.clone(),
            far: "sleep 304",
            armed: true,
        };
        let ender = Ender::new();
        let runner = {
            let ender = ender.clone();
            let script = format!("echo $$ > '{}'; exec sleep 304", pidfile.display());
            std::thread::spawn(move || {
                run_in(
                    &ender,
                    &mut (),
                    sh(&script),
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                    never,
                    |_| {},
                )
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while pid_in(&pidfile) <= 0 || ender.slot().is_none() {
            assert!(
                Instant::now() < deadline,
                "the child never came up: pid {}, in the slot {}",
                pid_in(&pidfile),
                ender.slot().is_some()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = pid_in(&pidfile);
        assert!(alive(pid), "positive half: it runs before end()");
        ender.end();
        assert!(!alive(pid), "the process {pid} outlived end()");
        guard.armed = false;
        // The thread is in its long look: it finds the slot empty at the end of it and is over.
        drop(runner);
    }

    /// An `end()` that comes first leaves nothing to end: the run does not even try to spawn (a command that
    /// cannot start would be `Failed`). Mutation: delete the look at the flag under the lock before the spawn
    /// and this is `Failed`.
    #[test]
    fn a_run_asked_to_end_before_it_started_spawns_nothing() {
        let ender = Ender::new();
        ender.end();
        let ran = run_in(
            &ender,
            &mut (),
            Command::new("/nonexistent/taw-bounded-test"),
            Duration::from_secs(20),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        assert!(matches!(ran, Ran::Ended), "{ran:?}");
        // The positive half: the same command through a run that was not ended is the failure of the test above.
        let fresh = run_in(
            &Ender::new(),
            &mut (),
            Command::new("/nonexistent/taw-bounded-test"),
            Duration::from_secs(20),
            Duration::from_millis(10),
            never,
            |_| {},
        );
        assert!(matches!(fresh, Ran::Failed(_)), "{fresh:?}");
    }
}
