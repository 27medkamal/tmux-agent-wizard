//! The nudge: how a report tells the watcher of its pane that there is something to read.
//!
//! A watcher reads its pane when the pane prints something, and on a recheck every few seconds
//! while an agent is in it, and never when the pane has no agent and prints nothing. A report an
//! agent makes of its own state is written to a pane option, which the watcher reads with the rest of
//! the pane, and an agent that reports is often quiet while it thinks: left to those reads the
//! first report of a pane would be noticed when the pane next printed, and the next after the recheck.
//! So `taw pane report-agent` tells the watcher. There is one mechanism:
//!
//! * The reporter sends one `SIGURG` to the pid in `@agent_watch_pid` ([`send`]). It reads that
//!   pid in the same tmux invocation that writes the report, so the nudge costs no tmux call.
//! * The watcher ([`subscribe`]) turns the signal into a wake-up of its loop, which makes the pane
//!   eligible for a read at its next tick, as output does ([`StreamState::nudge`](super::StreamState)).
//!
//! `SIGURG` because its default action is to be ignored: `@agent_watch_pid` is the pid of a process
//! that may have gone, and a number tmux remembers for a pane is not one to put `SIGUSR1`, which ends
//! a process that does not catch it, on. A process that is not a watcher takes the nudge for
//! nothing, and so does a watcher of an older `taw`, which then reads on its recheck as before.
//!
//! Signals reach a process, not a watcher, and a process has one handler for a signal. A watcher
//! is a process of its own (`taw watch run`), but a test runs several in one, so the handler wakes
//! every watcher of the process, which costs each a read at most. The handler itself does nothing but
//! write a byte to a socket the process owns, which is all a signal handler may do; a thread reads
//! the socket and does the rest.

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, Once};

use ::log::{debug, warn};

use super::Wake;

/// The signal a report sends.
const NUDGE: libc::c_int = libc::SIGURG;

/// The write end of the socket the handler writes to, `-1` until [`subscribe`] has made it.
static HANDLER_FD: AtomicI32 = AtomicI32::new(-1);

/// Where a nudge goes: the loop of each watcher in this process.
static LISTENERS: Mutex<Vec<Sender<Wake>>> = Mutex::new(Vec::new());

/// The process to signal for `pid`, the number `@agent_watch_pid` holds, or `None` when the number
/// names no single process of someone's: `0` is this process's own group, `1` is init, and a value
/// that does not fit a pid (tmux was given it by hand) is no watcher. The one place that says so, and
/// pure, so that it is tested with the numbers that must never be signalled without sending them.
fn target(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 1)
}

/// Tells the watcher that has process `pid` that something is owed a read, when `pid` is one to
/// signal ([`target`]). A process that has gone, or that is not allowed to be signalled, is no
/// error: there is nothing to do about it.
pub fn send(pid: u32) {
    let Some(pid) = target(pid) else {
        return;
    };
    // SAFETY: `kill` takes two integers and touches no memory of this process.
    let sent = unsafe { libc::kill(pid, NUDGE) };
    if sent != 0 {
        debug!(
            "could not nudge process {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Has this process's nudges wake `wake`'s loop: the first call installs the handler, and every call
/// adds a watcher. A watcher that has ended is forgotten at the next nudge. If the handler cannot
/// be installed the watcher simply is not nudged, and reads on its own schedule.
pub(super) fn subscribe(wake: Sender<Wake>) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        if let Err(err) = install() {
            warn!("could not listen for nudges from reports: {err}");
        }
    });
    LISTENERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(wake);
}

/// The handler, the socket it writes to and the thread that reads it.
fn install() -> std::io::Result<()> {
    let (reader, writer) = UnixStream::pair()?;
    // A full socket means a nudge is waiting already, and the handler must never block.
    writer.set_nonblocking(true)?;
    HANDLER_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
    // The handler writes to it for as long as the process lives.
    std::mem::forget(writer);

    // SAFETY: a zeroed `sigaction` is the empty one; the handler is an `extern "C"` function that
    // does only what a handler may, and the call is told what it is.
    let installed = unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_nudge as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // Restarted, so that a read or a wait in another thread is not cut short by a nudge.
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(NUDGE, &action, std::ptr::null_mut())
    };
    if installed != 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::thread::Builder::new()
        .name("taw-nudge".to_string())
        .spawn(move || relay(reader))?;
    Ok(())
}

/// The signal handler: one byte to the socket, and the error number left as it was found, since
/// the code this interrupted may be about to read it.
extern "C" fn on_nudge(_signal: libc::c_int) {
    let fd = HANDLER_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = 1u8;
    // SAFETY: `write` is one of the functions a signal handler may call, and `fd` is the
    // non-blocking end of a socket this process keeps open; the error number is read and put
    // back through the pointer the C library gives for this thread.
    unsafe {
        let errno = errno_location();
        let saved = if errno.is_null() { 0 } else { *errno };
        libc::write(fd, (&raw const byte).cast(), 1);
        if !errno.is_null() {
            *errno = saved;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: the caller is a signal handler on the thread whose error number this is.
    unsafe { libc::__errno_location() }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: the caller is a signal handler on the thread whose error number this is.
    unsafe { libc::__error() }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
)))]
unsafe fn errno_location() -> *mut libc::c_int {
    std::ptr::null_mut()
}

/// Reads what the handler writes and wakes every watcher, until the process ends.
fn relay(mut reader: UnixStream) {
    let mut buf = [0u8; 64];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => wake_all(),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn wake_all() {
    LISTENERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|wake| wake.send(Wake::Nudge).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_nudge_sent_to_this_process_wakes_a_subscribed_watcher() {
        let (tx, rx) = mpsc::channel();
        subscribe(tx);
        send(std::process::id());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Wake::Nudge)
        ));
    }

    #[test]
    fn a_watcher_that_has_ended_is_forgotten_and_the_others_still_wake() {
        let (gone, dropped) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        subscribe(gone);
        subscribe(tx);
        drop(dropped);
        send(std::process::id());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Wake::Nudge)
        ));
        // And again: the one that ended did not break the next.
        send(std::process::id());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Wake::Nudge)
        ));
    }

    #[test]
    fn a_nudge_does_not_end_a_process_that_is_no_watcher() {
        // The pid in `@agent_watch_pid` can be another process's by the time a report reads it.
        let mut bystander = Command::new("sleep").arg("30").spawn().unwrap();
        send(bystander.id());
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "the nudge ended it"
        );
        bystander.kill().unwrap();
        let _ = bystander.wait();
    }

    #[test]
    fn only_a_number_that_names_one_process_is_a_pid_to_signal() {
        // `0` is this process's group, `1` is init, and nothing above the largest pid fits one.
        for pid in [0, 1, u32::MAX, 1 << 31] {
            assert_eq!(target(pid), None, "{pid}");
        }
        let mine = std::process::id();
        assert_eq!(target(mine), Some(mine as libc::pid_t));
        assert_eq!(target(2), Some(2));
        assert_eq!(target(i32::MAX as u32), Some(i32::MAX));
    }
}
