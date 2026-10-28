// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Session state (snapshot/restore, with agent resume) and herdr's git-worktree workflow.
//!
//! herdr workspace = tmux session, tab = window, pane = pane.
//! tmux is the database: `taw session save` snapshots the live layout to
//! `<state>/sessions/<name>.json`; `taw session restore` recreates whatever the current
//! server is missing, relaunching agents into their native conversations when
//! `[session] resume_agents_on_restore` is set. `worktree` mirrors herdr's git-worktree
//! commands onto `git worktree` + a tmux session per worktree.

pub mod create;
pub mod history;
pub mod naming;
pub mod projects;
pub mod prompt;
pub mod restore;
pub mod snapshot;
pub mod worktree;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

use crate::paths::Paths;
use crate::tmux::Tmux;

const AUTOSAVE_MARKER: &str = ".autosave-requested";
const AUTOSAVE_SNAPSHOT_NAME: &str = "default";

/// The server option a restore sets while it runs, holding `<pid> <unix time>`: the process that is
/// restoring, and when it began ([`restoring_mark`]). Every new window and split fires the plugin's
/// autosave hooks, a restore makes dozens of them, and a snapshot taken between two of them holds a
/// half-made layout (the sessions that are still to come missing from it, the panes made so far with
/// no agent yet) and is written over the snapshot the restore is reading its layout from.
///
/// The restore clears it when it ends, and a restore that is killed (Ctrl-C, a closed terminal) does
/// not get to: that is what the process id is for, as [`restoring`] says.
pub const RESTORING_OPTION: &str = "@agent_wizard_restoring";

/// How long a value of [`RESTORING_OPTION`] counts at most. A process id can be given to another
/// process once the restore that had it has gone, which then looks as if the restore were still
/// running; a restore waits at most 15 seconds on any pane, so a mark ten minutes old was left by one
/// that is not running, whoever has its process id now.
const RESTORING_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// What a restore with process id `pid` sets [`RESTORING_OPTION`] to at unix time `since`.
pub fn restoring_mark(pid: u32, since: u64) -> String {
    format!("{pid} {since}")
}

/// The unix time now, in seconds.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Whether a restore is running on this server just now: [`RESTORING_OPTION`] is set, by a process
/// that is still there, not longer ago than [`RESTORING_STALE_AFTER`].
pub fn restoring(t: &Tmux) -> bool {
    let Ok(mark) = t.run(&["show-options", "-gqv", RESTORING_OPTION]) else {
        return false;
    };
    is_restoring(&mark, unix_now(), process_is_alive)
}

/// [`restoring`] for a mark that has been read: `<pid> <unix time>`, at unix time `now`, with
/// `alive` saying whether a process id is a process that is there.
///
/// A restore that was killed has no chance to clear what it set, so a mark whose process is gone
/// is not a restore that is running: autosave would otherwise stay off until the mark is old
/// enough to be ignored. A mark that is not `<pid> <unix time>` is no restore either.
fn is_restoring(mark: &str, now: u64, alive: impl Fn(u32) -> bool) -> bool {
    let mut fields = mark.split_whitespace();
    let (Some(pid), Some(since), None) = (fields.next(), fields.next(), fields.next()) else {
        return false;
    };
    let (Ok(pid), Ok(since)) = (pid.parse::<u32>(), since.parse::<u64>()) else {
        return false;
    };
    now.saturating_sub(since) < RESTORING_STALE_AFTER.as_secs() && alive(pid)
}

/// Whether `pid` is the id of a process that is there: signal 0 is sent to it, which delivers
/// nothing and only says whether it could be. A process that is there but belongs to someone else
/// answers that it is not permitted, which is a process all the same. `0` and `1` are no restore (the
/// first is this process's own group, the second is init) and a number that does not fit a pid was
/// put there by hand.
fn process_is_alive(pid: u32) -> bool {
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 1) else {
        return false;
    };
    // SAFETY: `kill` takes two integers and touches no memory of this process, and signal 0 sends nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // "Not permitted" is an answer too, and the process is there: a restore run by another user
    // (under sudo, say) answers it, and its mark must still hold autosave off. No portable test
    // has a process of another user to ask about, so this was checked by hand with the pid of a
    // root-owned process, which held autosave off, and the pid of one that had ended, which did not.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn autosave_marker_path(paths: &Paths) -> PathBuf {
    paths.sessions_dir.join(AUTOSAVE_MARKER)
}

/// Requests an autosave: touches `<sessions_dir>/.autosave-requested`. Cheap — called by
/// the watcher on every transition and by tmux hooks (`after-new-window`,
/// `after-split-window`, `after-kill-pane`, `session-closed`, via `taw session autosave`).
/// The actual save is debounced by [`autosave_if_due`].
pub fn autosave_touch(paths: &Paths) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.sessions_dir)
        .with_context(|| format!("creating {}", paths.sessions_dir.display()))?;
    let marker = autosave_marker_path(paths);
    std::fs::write(&marker, b"").with_context(|| format!("touching {}", marker.display()))
}

/// If an autosave was requested (the marker file exists) and the last "default" snapshot
/// is older than `min_interval` (or there is none yet), takes a fresh snapshot, saves it
/// as "default", removes the marker, and returns `true`. Otherwise does nothing and
/// returns `false`. Cheap when no autosave is due: only stats the marker and the existing
/// snapshot file before doing any tmux work, so it is safe to call frequently (the
/// watcher's loop, every matching tmux hook).
///
/// Never saves an empty layout: a live server briefly has no sessions while a
/// shutdown closes them one by one, and overwriting the last good snapshot with that
/// moment's emptiness would lose the whole layout for a state nothing asked to save. The
/// marker is left in place when this happens, so once something is actually there again
/// to save, the very next matching hook tries again.
///
/// Nor does it save while a restore is running ([`restoring`]): the hooks this is called from
/// fire on every window and split the restore makes, and what is saved then is a half-made
/// layout, written over the snapshot the restore is reading. The marker is left in place then
/// too, and the restore makes the one save that is due when it is done. These two are the
/// only cases where an autosave request is not consumed by this call.
pub fn autosave_if_due(
    t: &Tmux,
    cfg: &crate::config::Config,
    paths: &Paths,
    min_interval: Duration,
) -> anyhow::Result<bool> {
    let marker = autosave_marker_path(paths);
    if !marker.exists() {
        return Ok(false);
    }

    let snapshot_path = snapshot::snapshot_path(paths, AUTOSAVE_SNAPSHOT_NAME);
    if let Ok(modified) = std::fs::metadata(&snapshot_path).and_then(|m| m.modified()) {
        let age = std::time::SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age < min_interval {
            return Ok(false);
        }
    }

    if restoring(t) {
        return Ok(false);
    }

    // Checked with a bare `list-sessions`, not `Tmux::list_sessions` (which also runs
    // `list-windows -a` for each session's active window): with zero sessions, `-a`
    // has nothing to resolve a target against and fails outright, where a bare
    // `list-sessions` still exits 0 with empty output.
    if t.run(&["list-sessions", "-F", "#{session_id}"])?
        .trim()
        .is_empty()
    {
        return Ok(false);
    }
    let snap = snapshot::take(t)?;
    // The layout this replaces may be the last of a server that is gone: it is kept as a copy first,
    // whatever the age of the newest copy, when this server started after it was taken.
    snapshot::save_for_server(paths, AUTOSAVE_SNAPSHOT_NAME, &snap, t.server_start_unix())?;
    // The same opt-in the explicit `session save` honours: an autosave that skipped the
    // screens would mean the pane history only ever matched a hand-run save.
    write_history(t, cfg, paths, AUTOSAVE_SNAPSHOT_NAME, &snap);
    let _ = std::fs::remove_file(&marker);
    Ok(true)
}

// Follows herdr's src/app/session.rs and master 0ff0f27e22 (#4400, #4320
// in v0.9.2's changelog).
// Differences: behaviour only. A shutdown that closes panes one by one keeps the full layout;
// herdr's 48-snapshot history is in `snapshot::save`, and its save before a Linux shutdown is not
// ported (docs/PARITY.md, structural gaps).
/// [`autosave_if_due`] for the two tmux hooks that fire when something *closes*
/// (`after-kill-pane`, `session-closed`) rather than opens: waits `close_wait` first, then
/// skips entirely if tmux itself is no longer reachable, rather than racing a whole-server
/// shutdown to save whatever partial layout this one closed pane or session leaves behind.
///
/// `min_interval` and `close_wait` are both explicit parameters (never read from a clock
/// internally) so a test can use a short wait instead of the real five seconds; the tmux
/// hook installer is what supplies that real value (taw's shutdown protection, as herdr has;
/// the 48-snapshot history is kept by `snapshot::save` on every save).
///
/// A server that is merely down to zero sessions (not gone, just empty right now) also
/// fails the liveness probe this uses and so is skipped here — which is exactly the state
/// [`autosave_if_due`]'s own empty-layout guard would have refused to save regardless.
pub fn autosave_after_close(
    t: &Tmux,
    cfg: &crate::config::Config,
    paths: &Paths,
    min_interval: Duration,
    close_wait: Duration,
) -> anyhow::Result<bool> {
    let marker = autosave_marker_path(paths);
    if !marker.exists() {
        return Ok(false);
    }
    std::thread::sleep(close_wait);
    if t.list_sessions().is_err() {
        return Ok(false);
    }
    autosave_if_due(t, cfg, paths, min_interval)
}

/// Captures and writes the pane history for a snapshot just saved as `name`, or removes
/// any history already saved under that name when the feature is off.
///
/// Removing matters: turning `[experimental] pane_history` off has to stop the screens
/// being *kept*, not just stop new ones being written, or a file full of whatever was on
/// screen would sit in the state directory indefinitely after someone decided they did
/// not want that.
///
/// Failure is logged, never propagated: a snapshot whose layout was written is a
/// successful save, and the screens are the optional part.
pub fn write_history(
    t: &Tmux,
    cfg: &crate::config::Config,
    paths: &Paths,
    name: &str,
    snap: &snapshot::Snapshot,
) -> usize {
    if !cfg.experimental.pane_history {
        if let Err(err) = history::clear(paths, name) {
            log::warn!("could not remove the pane history of '{name}': {err:#}");
        }
        return 0;
    }
    let captured = history::capture(t, snap);
    let panes = captured
        .sessions
        .iter()
        .flat_map(|s| s.windows.iter())
        .map(|w| w.panes.len())
        .sum();
    if let Err(err) = history::save(paths, name, &captured) {
        log::warn!("could not save the pane history of '{name}': {err:#}");
        return 0;
    }
    panes
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn a_restore_is_running_when_the_process_that_marked_it_is_there_and_the_mark_is_recent() {
        assert_eq!(restoring_mark(4242, 7), "4242 7");
        assert!(is_restoring(&restoring_mark(4242, NOW - 5), NOW, |pid| {
            pid == 4242
        }));
        // The process the mark names is the one that is asked about.
        assert!(!is_restoring(&restoring_mark(4242, NOW - 5), NOW, |pid| {
            pid == 4243
        }));
    }

    #[test]
    fn a_restore_that_was_killed_is_not_running_however_recent_its_mark_is() {
        assert!(!is_restoring(&restoring_mark(4242, NOW), NOW, |_| false));
    }

    #[test]
    fn a_mark_ten_minutes_old_is_no_restore_even_when_its_process_id_is_in_use_again() {
        let stale = RESTORING_STALE_AFTER.as_secs();
        assert!(is_restoring(
            &restoring_mark(4242, NOW - (stale - 1)),
            NOW,
            |_| true
        ));
        assert!(!is_restoring(
            &restoring_mark(4242, NOW - stale),
            NOW,
            |_| true
        ));
    }

    #[test]
    fn a_mark_that_is_not_a_process_id_and_a_time_is_no_restore() {
        for mark in [
            "",
            "   ",
            // What a restore that set nothing but the time would have left.
            "1790826312",
            "4242",
            "x 1790826312",
            "4242 x",
            "-4 1790826312",
            "4242 -1",
            "4242 1790826312 7",
            "4242,1790826312",
        ] {
            assert!(!is_restoring(mark, NOW, |_| true), "{mark:?}");
        }
    }

    #[test]
    fn a_mark_made_by_a_clock_that_is_ahead_of_this_one_still_counts() {
        assert!(is_restoring(&restoring_mark(4242, NOW + 30), NOW, |_| true));
    }

    #[test]
    fn a_process_id_is_alive_when_a_process_has_it() {
        assert!(process_is_alive(std::process::id()));

        // No process is `0` or `1` as far as this is concerned, and a number that does not fit a
        // pid names none.
        for pid in [0, 1, u32::MAX] {
            assert!(!process_is_alive(pid), "{pid}");
        }

        // One that has ended, and been waited for, has not.
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let pid = ended.id();
        ended.wait().unwrap();
        assert!(!process_is_alive(pid));
    }
}
