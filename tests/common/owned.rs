//! The guard of the processes an app test starts or reads from a record, and the reading of a status a pane's
//! shell wrote. Shared by `app_local.rs` and `app_remote.rs`: it is the one place that signals a pid, only a
//! pid it recorded with its start time, never a name.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use super::taw_bin;

/// The status the pane's shell wrote (`echo $? >path`), or `None` until its line is whole: the shell creates
/// the file empty before it writes to it, so a file that is there is not yet a status.
pub fn status_of(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|text| text.ends_with('\n'))
}

/// The names in an app records folder that are records: whole `.json` files. The app writes a record as a dot
/// file (`.<pid>.json.<pid>.<n>.tmp`) and renames it, so a folder read at the wrong moment holds the temporary
/// name alone, and that is not a record.
pub fn record_names(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
        .collect()
}

/// Whether `pid` is a process that runs.
pub fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only asks.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Sends `signal` to `pid`, which this test read from a record of the app it started: never a name.
pub fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: a pid of a process of this test's own fixture.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

thread_local! {
    /// The processes this test read from a record, for [`Owned`] to end: each pid with the start time it had when
    /// it was read, so that a pid that has been given to another process since is not taken for it, and the
    /// word its command line must hold (this build's `taw`, or what [`own_as`] was given).
    static OWNED: std::cell::RefCell<Vec<(u32, String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// When `pid` started, as `ps` says it (`lstart`), or empty when there is no such process.
pub fn start_time_of(pid: u32) -> String {
    std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The command line of `pid` as `ps` says it, or empty when there is no such process.
pub fn args_of(pid: u32) -> String {
    std::process::Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether a process is the one that was recorded: it started when the record says (an empty record, from a
/// process that was already gone, matches nothing) and its command line still holds `needle` (this build's
/// `taw`, or the word a test gave for a process that is not `taw`).
pub fn is_the_recorded_process(
    recorded_start: &str,
    start_now: &str,
    args_now: &str,
    needle: &str,
) -> bool {
    !recorded_start.is_empty() && recorded_start == start_now && args_now.contains(needle)
}

/// Ends, when the test is over, the processes it read from a record (the supervisor and the child) that
/// still run and are still what was recorded. By pid, never by name; made first in a test that kills or
/// starts a supervisor or a child. A pid whose process is not `taw`'s (the pane's shell that runs the app
/// by hand) is left to the terminal's server, which ends with the test.
pub struct Owned;

impl Owned {
    pub fn new() -> Owned {
        OWNED.with(|owned| owned.borrow_mut().clear());
        Owned
    }
}

/// The start time of `pid` from `read`, tried again every `every` for up to `up_to` while the process is alive
/// and the read comes back empty (a `ps` that fails to fork under load says nothing, and an empty record would
/// make the guard leave a live process alone for good). A process that is already gone has none, and needs
/// none. One that is alive and still reads empty after that is a panic naming it, so a leak is loud.
pub fn start_time_when_alive(
    pid: u32,
    mut read: impl FnMut(u32) -> String,
    alive: impl Fn(u32) -> bool,
    every: Duration,
    up_to: Duration,
) -> String {
    let until = Instant::now() + up_to;
    loop {
        let started = read(pid);
        if !started.is_empty() || !alive(pid) {
            return started;
        }
        assert!(
            Instant::now() < until,
            "the start time of the live process {pid} reads empty for {up_to:?}: {}",
            args_of(pid)
        );
        std::thread::sleep(every);
    }
}

/// Records `pid`, a process of this build's `taw`, for [`Owned`], with the start time it has now.
pub fn own(pid: u32) {
    own_as(pid, taw_bin());
}

/// Records `pid` for [`Owned`], with the start time it has now, for a process that is not `taw` but whose
/// command line holds `needle` when the guard looks (a stand-in that ran `exec sleep 600` holds `sleep 600`).
pub fn own_as(pid: u32, needle: &str) {
    OWNED.with(|owned| {
        let mut owned = owned.borrow_mut();
        if !owned.iter().any(|(known, _, _)| *known == pid) {
            let started = start_time_when_alive(
                pid,
                start_time_of,
                alive,
                Duration::from_millis(50),
                Duration::from_secs(5),
            );
            owned.push((pid, started, needle.to_string()));
        }
    });
}

/// Takes `pid` off the guard: the test has seen it gone.
pub fn disown(pid: u32) {
    OWNED.with(|owned| owned.borrow_mut().retain(|(known, _, _)| *known != pid));
}

impl Drop for Owned {
    fn drop(&mut self) {
        OWNED.with(|owned| {
            for (pid, started, needle) in owned.borrow_mut().drain(..) {
                if alive(pid)
                    && is_the_recorded_process(
                        &started,
                        &start_time_of(pid),
                        &args_of(pid),
                        &needle,
                    )
                {
                    signal(pid, libc::SIGKILL);
                }
            }
        });
    }
}
