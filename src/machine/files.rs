//! What is kept on disk about machines, under `<state>/machines`: the file the keeper app writes about
//! each machine ([`MachineFile`], read for `taw machine list`), and what the far side of a connection
//! keeps for the watcher and the key rule: the lease, the walk and the events the feed tails.
//!
//! Every field of what is read has a default, so a file written by an older or a newer taw still reads,
//! and a file that is not there, is empty or is not JSON counts as none.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::wire;
use super::{MachineView, words};
use crate::paths::Paths;

/// The app's hold on every machine's connection, and what it knows of each ([`super::machines`]).
pub use super::machines::Machines;

/// How long ago a machine may last have been heard from and still count as live, in milliseconds.
pub const LIVE_WITHIN_MS: u64 = 15_000;

/// `<state>/machines/<name>.json`: what the app that keeps the connections last knew of one machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineFile {
    pub name: String,
    /// The machine's last state as its feed sent it, or null. A state that does not read as one (a file
    /// of another taw, or of a half-written feed) is none: the rest of the file is kept.
    #[serde(deserialize_with = "lenient_state")]
    pub state: Option<wire::State>,
    /// Unix milliseconds of the last state received.
    pub received_at: Option<u64>,
    /// Unix milliseconds of the last line of any kind heard from the machine.
    pub last_seen: Option<u64>,
    /// Whether the keeper held a connection to the machine when it wrote this. Not enough to be live:
    /// see [`is_live`].
    pub live: bool,
    /// What the header says of the machine (`connected: taw 0.1.0`, `unreachable: ...`).
    pub words: String,
    /// The short form of `words` when they wait for the person to act, else empty ([`MachineView::short`]).
    pub short: String,
    pub taw_version: Option<String>,
    pub protocol: Option<u32>,
    /// The machine turned out to be this machine's own tmux server.
    pub this_server: bool,
}

/// A `state` field that reads as a [`wire::State`], or `None` when it is null or does not type.
fn lenient_state<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<wire::State>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// The most a machine file may hold, in bytes: far above any real state, so a longer file is not ours.
const READ_CAP: u64 = 16 * 1024 * 1024;

/// The file of the machine called `name`, or `None` when there is none or it cannot be read.
pub fn read(paths: &Paths, name: &str) -> Option<MachineFile> {
    // A name that is not a machine's has no file (`Paths::machine_file`).
    let text = read_capped(&paths.machine_file(name)?)?;
    // Read as a value first: serde would take a JSON array for a struct with every field defaulted.
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if !value.is_object() {
        return None;
    }
    let file: MachineFile = serde_json::from_value(value).ok()?;
    // A file that says it is another machine's is not this one's; one that says nothing is accepted.
    (file.name.is_empty() || file.name == name).then_some(file)
}

/// Opens `path` the way `options` say, without waiting, and only when what is there is a regular file (a
/// link to one is fine). A FIFO or a device is an error: a FIFO put there after a look at the path would
/// otherwise block the open for as long as nothing is at its other end, and a device would be read or
/// written as if it were a file. The look is at the open file, not at the path, so nothing can be put in
/// between. The one place machine files are opened.
fn open_regular(options: &mut std::fs::OpenOptions, path: &Path) -> std::io::Result<std::fs::File> {
    let file = options.custom_flags(libc::O_NONBLOCK).open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(std::io::Error::other("not a regular file"))
    }
}

/// The text of a regular file no longer than [`READ_CAP`]. A FIFO, a device, a directory or a longer file
/// is none: nothing blocks and nothing grows without bound.
fn read_capped(path: &Path) -> Option<String> {
    let file = open_regular(std::fs::OpenOptions::new().read(true), path).ok()?;
    let mut text = String::new();
    file.take(READ_CAP + 1).read_to_string(&mut text).ok()?;
    (text.len() as u64 <= READ_CAP).then_some(text)
}

/// The lock an app holds while it keeps the connections: held for as long as this value lives, and let go
/// when it is dropped or its process ends, however it ends.
#[derive(Debug)]
pub struct Keeper {
    _lock: std::fs::File,
}

/// Takes the keeper lock (`flock`, without waiting): `Some` for the one app that gets it, `None` for every
/// other while it is held. The lock file is made when it is not there.
pub fn take_keeper(paths: &Paths) -> Option<Keeper> {
    let path = paths.keeper_lock_file();
    std::fs::create_dir_all(paths.machines_dir()).ok()?;
    let file = open_regular(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600),
        &path,
    )
    .ok()?;
    // SAFETY: flock on a descriptor this function owns; the lock goes with the descriptor.
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0)
        .then_some(Keeper { _lock: file })
}

/// Whether an app holds the keeper lock, so that the machine files are being kept up to date. Only looks:
/// it makes neither the folder nor the lock file, and a lock file that is not there, or cannot be opened,
/// says no. (For a moment while it looks, the lock is shared, so that an app taking it then tries again
/// at its next turn.)
pub fn keeper_alive(paths: &Paths) -> bool {
    let Ok(file) = open_regular(
        std::fs::OpenOptions::new().read(true),
        &paths.keeper_lock_file(),
    ) else {
        return false;
    };
    // SAFETY: flock on a descriptor this function owns, which is closed (and so unlocked) on return.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    !taken && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
}

/// Writes the file of the machine `file` names, whole. A name that is not a machine's has none.
pub fn write_machine_file(paths: &Paths, file: &MachineFile) {
    let Some(path) = paths.machine_file(&file.name) else {
        return;
    };
    if let Err(err) = write_json(&path, file) {
        log::warn!("could not write the file of {}: {err}", file.name);
    }
}

/// What the list knows of the machine `name` from its `file` (none: nothing yet, so `connecting`), as the
/// app that does not keep the connections, and the popup, read it: the words only while a keeper keeps
/// the file, and live only when [`is_live`] says so. `keeper` is [`keeper_alive`]'s answer.
pub fn view_of(file: Option<&MachineFile>, name: &str, keeper: bool, now_ms: u64) -> MachineView {
    let Some(file) = file else {
        return MachineView {
            name: name.to_string(),
            words: words::connecting(None),
            ..MachineView::default()
        };
    };
    let shown = words_shown(file, keeper);
    let words = if shown.is_empty() {
        words::connecting(
            file.last_seen
                .map(|seen| now_ms.saturating_sub(seen) / 1000),
        )
    } else {
        shown.to_string()
    };
    // Only the words a keeper's file says: the `connecting` made up above has no short form.
    let short = if shown.is_empty() {
        String::new()
    } else {
        file.short.clone()
    };
    MachineView {
        name: name.to_string(),
        live: is_live(file, keeper, now_ms),
        words,
        short,
        state: file.state.clone(),
        last_seen: file.last_seen,
        this_server: file.this_server,
        no_keeper: false,
    }
}

/// Live means connected and heard from lately: a file that says `live` with a `last_seen` older than
/// [`LIVE_WITHIN_MS`] (or none), or that nobody is keeping up to date, is not.
pub fn is_live(file: &MachineFile, keeper: bool, now_ms: u64) -> bool {
    keeper
        && file.live
        && file
            .last_seen
            .is_some_and(|seen| now_ms.abs_diff(seen) <= LIVE_WITHIN_MS)
}

/// What the header says of the machine: the file's words while a keeper keeps the file, and nothing
/// without one (a keeper that died leaves its last words behind, and they would still say
/// `connected`). `keeper` is [`keeper_alive`]'s answer.
pub fn words_shown(file: &MachineFile, keeper: bool) -> &str {
    if keeper { &file.words } else { "" }
}

/// How long a lease counts as held after it was last refreshed.
pub const LEASE_FRESH: Duration = Duration::from_secs(15);

/// The largest the events file may grow before the next append starts it again.
pub const EVENTS_CAP: u64 = 256 * 1024;

/// `<state>/machines/lease.json`: which app is looking at this machine's attention, and when it last said so.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lease {
    /// `<host>-<pid>` of the app.
    pub holder: String,
    /// Unix milliseconds.
    pub refreshed_at: u64,
}

/// The lease on file, or `None` when there is none or it cannot be read.
pub fn read_lease(paths: &Paths) -> Option<Lease> {
    read_object(&paths.lease_file())
}

/// Whether a lease on file was refreshed less than [`LEASE_FRESH`] from `now_ms`, on either side of it (a
/// clock that is far ahead is no better than one that is behind).
pub fn lease_fresh(paths: &Paths, now_ms: u64) -> bool {
    read_lease(paths).is_some_and(|lease| fresh(&lease, now_ms))
}

fn fresh(lease: &Lease, now_ms: u64) -> bool {
    now_ms.abs_diff(lease.refreshed_at) < LEASE_FRESH.as_millis() as u64
}

/// Takes the lease for `holder`, or keeps it: it is written when there is none, when the one on file is
/// stale, or when it is `holder`'s own (which refreshes it). Another holder's fresh lease is left as it is.
///
/// The file is the truth and the returned bool is advisory: it says whether `holder` held the lease when
/// this looked and wrote. Two feeds that both see a stale lease both write and both get `true`, and the
/// last rename wins, so whoever needs to know who holds it reads the file again ([`read_lease`]).
///
/// A fresh lease whose holder is gone ([`holder_gone`]: the app ended, however it ended, and with it whatever spoke
/// for it here) is no one's: the next app takes it at once, without waiting out [`LEASE_FRESH`].
pub fn offer_lease(paths: &Paths, holder: &str, now_ms: u64) -> bool {
    if let Some(lease) = read_lease(paths)
        && lease.holder != holder
        && fresh(&lease, now_ms)
        && !holder_gone(paths, &lease.holder)
    {
        return false;
    }
    let lease = Lease {
        holder: holder.to_string(),
        refreshed_at: now_ms,
    };
    match write_json(&paths.lease_file(), &lease) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("could not write the lease: {err}");
            false
        }
    }
}

/// Gives up the lease when `holder` holds it: the file is removed, so that the next app takes it at once. Another
/// holder's lease is left as it is. The read and the remove are not one step: a lease another app wrote between them
/// is removed too, and that app writes it again with its next lease line, a few seconds later.
pub fn release_lease(paths: &Paths, holder: &str) {
    if read_lease(paths).is_some_and(|lease| lease.holder == holder)
        && let Err(err) = std::fs::remove_file(paths.lease_file())
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not give up the lease: {err}");
    }
}

/// `holder` as a file name: every character outside `[A-Za-z0-9._-]` made `_`. `None` for an empty holder, or one
/// of dots only (a folder's name), which names no file.
fn holder_file_name(holder: &str) -> Option<String> {
    if holder.is_empty() {
        return None;
    }
    let safe: String = holder
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!safe.chars().all(|c| c == '.')).then_some(safe)
}

/// `<state>/machines/holders/<holder>.lock`, of [`HolderLock`].
fn holder_lock_path(paths: &Paths, holder: &str) -> Option<PathBuf> {
    Some(
        paths
            .machines_dir()
            .join("holders")
            .join(format!("{}.lock", holder_file_name(holder)?)),
    )
}

/// The lock that says a lease holder is still there: held (`flock`) by whatever speaks for the holder on this
/// machine, the feed of the app's connection on a machine and the keeper app itself on this one, for as long as
/// it runs. The kernel lets it go when that process ends, however it ends (a feed whose ssh went with a killed
/// app, a `SIGKILL`). Dropped, it gives up the holder's lease ([`release_lease`]) and removes its file.
#[derive(Debug)]
pub struct HolderLock {
    holder: String,
    path: PathBuf,
    lease: PathBuf,
    _lock: std::fs::File,
}

impl HolderLock {
    /// The holder this lock is for.
    pub fn holder(&self) -> &str {
        &self.holder
    }
}

impl Drop for HolderLock {
    fn drop(&mut self) {
        if read_object::<Lease>(&self.lease).is_some_and(|lease| lease.holder == self.holder) {
            let _ = std::fs::remove_file(&self.lease);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Takes the lock of `holder` ([`HolderLock`]), without waiting. `None` when it cannot be made or another process
/// holds it (two feeds of one app, for a moment while one replaces the other: the first keeps it).
pub fn take_holder_lock(paths: &Paths, holder: &str) -> Option<HolderLock> {
    let path = holder_lock_path(paths, holder)?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = open_regular(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600),
        &path,
    )
    .ok()?;
    // SAFETY: flock on a descriptor this function owns; the lock goes with the descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return None;
    }
    // A file removed between the open and the lock (a holder letting go, or [`remove_old_holder_files`]) is locked by
    // nobody who looks: the path must still name the file this holds.
    let same = |held: &std::fs::Metadata, named: &std::fs::Metadata| {
        held.dev() == named.dev() && held.ino() == named.ino()
    };
    if !matches!((file.metadata(), std::fs::symlink_metadata(&path)), (Ok(held), Ok(named)) if same(&held, &named))
    {
        return None;
    }
    remove_old_holder_files(paths, &path);
    Some(HolderLock {
        holder: holder.to_string(),
        path,
        lease: paths.lease_file(),
        _lock: file,
    })
}

/// How old a holder's lock file is (it is never written after it is made) when [`remove_old_holder_files`] removes it,
/// if no process holds it.
const HOLDER_FILE_KEPT: Duration = Duration::from_secs(5 * 60);

/// Removes the lock files of the holders folder, `keep` apart, that were made more than [`HOLDER_FILE_KEPT`] ago and
/// that no process holds: those of apps that were killed, or ended before their feed let go, so that each app run does
/// not leave one more for good. A held file is never removed, however old (an app that has run for days), and neither
/// is the file of the holder the lease names: it says that holder is gone, so that the next app takes its lease at
/// once ([`offer_lease`]), and it goes once the lease names someone else. Each is removed while this holds its lock, so
/// that no holder takes it in between. Errors are ignored: a file that stays is removed by the next lock taken.
fn remove_old_holder_files(paths: &Paths, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(paths.machines_dir().join("holders")) else {
        return;
    };
    let leased = read_lease(paths).and_then(|lease| holder_lock_path(paths, &lease.holder));
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || leased.as_ref() == Some(&path) {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .and_then(|made| made.elapsed().ok())
            .is_some_and(|age| age > HOLDER_FILE_KEPT);
        if !old {
            continue;
        }
        let Ok(file) = open_regular(std::fs::OpenOptions::new().read(true), &path) else {
            continue;
        };
        // SAFETY: flock on a descriptor this function owns, which is closed (and so unlocked) after the remove.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether `holder` is gone: its lock file ([`HolderLock`]) is there and no process holds it. A holder with no lock
/// file is not known to be gone (a taw from before the lock, or one that has just let go and removed it, having
/// given up its lease as it did): its lease counts as long as it is fresh.
pub fn holder_gone(paths: &Paths, holder: &str) -> bool {
    let Some(path) = holder_lock_path(paths, holder) else {
        return false;
    };
    let Ok(file) = open_regular(std::fs::OpenOptions::new().read(true), &path) else {
        return false;
    };
    // SAFETY: flock on a descriptor this function owns, which is closed (and so unlocked) on return.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) == 0 }
}

/// `<state>/machines/walks/<holder>.json`: the walk of every machine's stops one app last sent, in the app's names.
/// One file for each holder, so that the apps that write on one machine (two Macs' feeds, a keeper and a follower)
/// never overwrite each other's walk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Walk {
    /// The name the app gives this machine: the prefix of its own ids in `stops`.
    pub you: String,
    pub stops: Vec<String>,
    /// Unix milliseconds.
    pub at: u64,
    /// The lease holder (`<host>-<pid>`) whose walk this is: the app that wrote it, or on a machine the holder of
    /// the lease its feed last heard. Empty only in a file that was written by hand: such a walk is no one's.
    pub holder: String,
}

/// How old a walk file of another holder is when [`write_walk`] removes it.
const WALK_FILE_KEPT: Duration = Duration::from_secs(5 * 60);

/// The file of `holder`'s walk: its name with every character outside `[A-Za-z0-9._-]` made `_`. `None` for an
/// empty holder, which names no file.
pub fn walk_path(paths: &Paths, holder: &str) -> Option<PathBuf> {
    Some(
        paths
            .walks_dir()
            .join(format!("{}.json", holder_file_name(holder)?)),
    )
}

/// Writes the walk of `holder` that the app sent, to its own file, and removes the walk files of other holders
/// that nobody has written for [`WALK_FILE_KEPT`], so that restarts do not pile files up. An empty holder writes
/// nothing.
pub fn write_walk(paths: &Paths, holder: &str, you: &str, stops: &[String], now_ms: u64) {
    let Some(path) = walk_path(paths, holder) else {
        return;
    };
    let walk = Walk {
        holder: holder.to_string(),
        you: you.to_string(),
        stops: stops.to_vec(),
        at: now_ms,
    };
    if let Err(err) = write_json(&path, &walk) {
        log::warn!("could not write the walk: {err}");
        return;
    }
    remove_old_walks(paths, &path);
}

/// Removes the files of the walks folder, `keep` apart, that were last written more than [`WALK_FILE_KEPT`] ago.
/// Errors are ignored: a file that stays is removed by the next write.
fn remove_old_walks(paths: &Paths, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(paths.walks_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .and_then(|written| written.elapsed().ok())
            .is_some_and(|age| age > WALK_FILE_KEPT);
        if old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The walk `holder` wrote, or `None` when there is none, it cannot be read, or the file names another holder
/// (two holders whose names are alike to the file name never serve each other's walk). An empty holder has none.
pub fn read_walk(paths: &Paths, holder: &str) -> Option<Walk> {
    let walk: Walk = read_object(&walk_path(paths, holder)?)?;
    (walk.holder == holder).then_some(walk)
}

/// The longest the events file may sit unwritten past [`EVENTS_CAP`] before the next append starts it
/// again: every reader polls every 500 ms, so each has read to its end by then. The feed looks at the
/// file every tick (500 ms), so two seconds of quiet means no reader is behind. A reader slower than
/// that, while the file is past the cap, loses the lines it had not read; the cap is far above any real
/// number of toasts, so that takes a reader that has stalled.
pub const EVENTS_QUIET: Duration = Duration::from_secs(2);

/// The size past which the events file is emptied by the next append whatever its readers have read.
pub const EVENTS_HARD_CAP: u64 = 1024 * 1024;

/// Adds one line to the events file, under a lock so that two writers never mix their lines. A file
/// past [`EVENTS_CAP`] that nobody has written for [`EVENTS_QUIET`] (or past [`EVENTS_HARD_CAP`]) is
/// emptied first: whoever tails it notices that it is shorter than where it was.
pub fn append_event(paths: &Paths, line: &wire::Down) {
    append_event_at(paths, line, now_ms());
}

/// [`append_event`] with the clock given.
pub fn append_event_at(paths: &Paths, line: &wire::Down, now_ms: u64) {
    if let Err(err) = append_to(&paths.events_file(), &wire::render(line), now_ms) {
        log::warn!("could not write an event: {err}");
    }
}

fn append_to(path: &Path, text: &str, now_ms: u64) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Private to the user, as the crash records are: a toast holds an agent's title and its words.
    let mut file = open_regular(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600),
        path,
    )?;
    // The lock keeps the look at the size, the emptying and the append one step for a writer, so that
    // another writer's line is never emptied away between them (no test can show this).
    // SAFETY: flock on a descriptor this function owns; the lock goes with the descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let result = (|| {
        let meta = file.metadata()?;
        let written_ms = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(now_ms, |since| since.as_millis() as u64);
        let quiet = now_ms.saturating_sub(written_ms) >= EVENTS_QUIET.as_millis() as u64;
        if meta.len() > EVENTS_HARD_CAP || (meta.len() > EVENTS_CAP && quiet) {
            file.set_len(0)?;
        }
        file.write_all(text.as_bytes())
    })();
    // SAFETY: as above.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    result
}

/// A reader of the events file that has read up to where it is.
pub struct EventTail {
    path: PathBuf,
    offset: u64,
}

impl EventTail {
    /// Starts at the end of the file: what was written before is not news.
    pub fn at_end(paths: &Paths) -> EventTail {
        let path = paths.events_file();
        let offset = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        EventTail { path, offset }
    }

    /// The lines written since the last read, as far as they are whole. A file shorter than where this
    /// was has been started again: it is read from its start. A line that is not one the feed knows is
    /// skipped.
    pub fn read_new(&mut self) -> Vec<wire::Down> {
        let Ok(mut file) = open_regular(std::fs::OpenOptions::new().read(true), &self.path) else {
            return Vec::new();
        };
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return Vec::new();
        };
        if len < self.offset {
            self.offset = 0;
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file
            .take(len - self.offset)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Vec::new();
        }
        // Only up to the last newline: the rest is a line being written.
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        self.offset += end as u64 + 1;
        bytes[..end]
            .split(|b| *b == b'\n')
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter_map(|line| wire::parse_down(line).ok())
            .collect()
    }
}

/// `<state>/apps/<pid>.json`: what a running app child tells the supervisor above it and the one key rule
/// ([`crate::cli::ui_open`]): which client it runs, on which terminal, and whose machine it is inside.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppRecord {
    /// The app child's own process id (and the name of the file).
    pub pid: u32,
    /// The supervisor that started it.
    pub supervisor: u32,
    /// The attach it is waiting for, as a process id; 0 while it draws the list.
    pub client_pid: u32,
    /// The terminal that attach runs on, as `ttyname` gives it (`/dev/ttys004`), or empty.
    pub client_tty: String,
    /// The machine the attach goes to, or empty for this Mac.
    pub inside: String,
    /// The app's lease holder, `<host>-<pid>` ([`super::conn::holder_word`]).
    pub holder: String,
    /// Unix milliseconds.
    pub started_at: u64,
}

/// Whether an app record's process is there. A pid of 0 or 1 is no app (the field was missing, or it is
/// not a process of ours), and neither is one above `i32::MAX`, which is no pid at all (the watcher's
/// own check takes it for a process that is there).
pub fn app_alive(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32 && crate::watch::process_alive(pid)
}

/// Writes the record of the app `record.pid`, whole.
pub fn write_app_record(paths: &Paths, record: &AppRecord) {
    if let Err(err) = write_json(&paths.app_record(record.pid), record) {
        log::warn!("could not write the record of app {}: {err}", record.pid);
    }
}

/// The record of the app whose pid is `pid`, alive or not, or `None` when there is none or it cannot be
/// read. Only looks: the supervisor reads the record of a child that has just died.
pub fn read_app_record(paths: &Paths, pid: u32) -> Option<AppRecord> {
    let record: AppRecord = read_object(&paths.app_record(pid))?;
    (record.pid == pid).then_some(record)
}

/// Every record of an app that is alive, by pid. A record whose process is gone is not counted: it is
/// removed as it is read (an app that was killed leaves one) unless its supervisor is still there, which
/// reads it next to see which client the dead child had and removes it itself. A file that is not a
/// record, or is another pid's, is left alone and not counted. Looking makes no folder.
pub fn read_app_records(paths: &Paths) -> Vec<AppRecord> {
    let Ok(dir) = std::fs::read_dir(paths.apps_dir()) else {
        return Vec::new();
    };
    let mut records: Vec<AppRecord> = dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let pid: u32 = name.strip_suffix(".json")?.parse().ok()?;
            let record = read_app_record(paths, pid)?;
            if app_alive(pid) {
                Some(record)
            } else {
                if !app_alive(record.supervisor) {
                    remove_app_record(paths, pid);
                }
                None
            }
        })
        .collect();
    records.sort_by_key(|record| record.pid);
    records
}

/// Removes the record of the app `pid`; one that is not there is no error.
pub fn remove_app_record(paths: &Paths, pid: u32) {
    if let Err(err) = std::fs::remove_file(paths.app_record(pid))
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not remove the record of app {pid}: {err}");
    }
}

/// `<state>/machines/attached/<tty file name>.json`: a client of this machine's tmux that a person's app on
/// another machine started with `taw machine attach`, as that command left it. The key rule
/// ([`app_client`]) reads it to send the client back to the app's list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachedRecord {
    /// The terminal the client runs on, as `ttyname` gives it (`/dev/ttys004`).
    pub tty: String,
    /// The lease holder of the app that attached it, `<host>-<pid>` ([`super::conn::holder_word`]).
    pub holder: String,
    /// Unix milliseconds.
    pub at: u64,
    /// The process that attached: the tmux client itself, as `taw machine attach` becomes it by `exec`,
    /// which tells a client that took over the terminal's name later from the one this record is for. 0
    /// when the record does not say, which matches no client.
    pub pid: u32,
}

/// Writes the record of the client on `record.tty`, whole. A record with no terminal is not written.
pub fn write_attached(paths: &Paths, record: &AttachedRecord) -> std::io::Result<()> {
    let path = paths
        .attached_record(&record.tty)
        .ok_or_else(|| std::io::Error::other("no terminal to record"))?;
    write_json(&path, record)
}

/// The record of the client on `tty`, or `None` when there is none or it cannot be read. Only looks (the
/// tests' reader; the key rule reads them all, [`read_attached_records`]).
#[cfg(test)]
pub(crate) fn read_attached(paths: &Paths, tty: &str) -> Option<AttachedRecord> {
    let record: AttachedRecord = read_object(&paths.attached_record(tty)?)?;
    (record.tty == tty).then_some(record)
}

/// Every record under `attached/`, by terminal. A file that is not a record, or is another terminal's,
/// is left alone and not counted. Looking makes no folder.
pub fn read_attached_records(paths: &Paths) -> Vec<AttachedRecord> {
    let Ok(dir) = std::fs::read_dir(paths.attached_dir()) else {
        return Vec::new();
    };
    let mut records: Vec<AttachedRecord> = dir
        .flatten()
        .filter_map(|entry| {
            let text = read_capped(&entry.path())?;
            let record: AttachedRecord = serde_json::from_str(&text).ok()?;
            (paths.attached_record(&record.tty)? == entry.path()).then_some(record)
        })
        .collect();
    records.sort_by(|a, b| a.tty.cmp(&b.tty));
    records
}

/// Removes the record of the client on `tty`; one that is not there is no error.
pub fn remove_attached(paths: &Paths, tty: &str) {
    let Some(path) = paths.attached_record(tty) else {
        return;
    };
    remove_record_file(&path, tty);
}

fn remove_record_file(path: &Path, tty: &str) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not remove the record of the client on {tty}: {err}");
    }
}

/// How old a record is when its client may be told gone: `taw machine attach` writes it before it becomes
/// tmux, and the new client is not on the server's list until it is on a session.
const ATTACHED_GRACE_MS: u64 = 10_000;

/// Whether the record, which no client of tmux runs on the terminal of, is one nothing needs any more: the
/// process that attached (the tmux client itself, by `exec`) is not alive, and the record is older than
/// [`ATTACHED_GRACE_MS`] (a clock that went back keeps it). A live pid keeps the record whatever the
/// terminal says: that is a client the server asked here does not list (it is another server's, or has not
/// connected yet), and pid 0 counts as not alive.
fn attached_record_is_dead(record: &AttachedRecord, now_ms: u64) -> bool {
    !app_alive(record.pid) && now_ms.saturating_sub(record.at) > ATTACHED_GRACE_MS
}

/// Removes the file of `judged` only when it still holds that very record: one written since (a new
/// attach on the same terminal) is not the one judged, and one that is gone needs nothing. The read and then
/// the remove are not a compare-and-remove: a record written between them, in the microseconds between the read
/// and the unlink, is removed with the old one, and that fails closed (a client with no record gets the popup, and is
/// never sent back to an app it is not attached for).
fn remove_attached_if_unchanged(paths: &Paths, judged: &AttachedRecord) {
    let Some(path) = paths.attached_record(&judged.tty) else {
        return;
    };
    if read_object::<AttachedRecord>(&path).as_ref() == Some(judged) {
        remove_record_file(&path, &judged.tty);
    }
}

/// The attached records whose client is still there. A record whose terminal no client of `tmux` runs on
/// is removed as it is read (the client ended, and nothing takes its record away but this) only when
/// [`attached_record_is_dead`] says so and the file is still that record; otherwise it stays and is not
/// returned. When tmux cannot say which clients there are, no record is judged and none is removed.
fn attached_with_a_client(paths: &Paths, tmux: &crate::tmux::Tmux) -> Vec<AttachedRecord> {
    attached_with_a_client_at(paths, tmux, now_ms(), &|_| {})
}

/// [`attached_with_a_client`] at the time `now_ms`, calling `judged` with each record it has decided to
/// remove, before the file is read again for the removal (a seam for the test of a record that is replaced
/// in between).
fn attached_with_a_client_at(
    paths: &Paths,
    tmux: &crate::tmux::Tmux,
    now_ms: u64,
    judged: &dyn Fn(&AttachedRecord),
) -> Vec<AttachedRecord> {
    let records = read_attached_records(paths);
    if records.is_empty() {
        return records;
    }
    let Ok(clients) = tmux.list_clients() else {
        return records;
    };
    records
        .into_iter()
        .filter(|record| {
            let there = clients.iter().any(|client| client.client_tty == record.tty);
            if !there && attached_record_is_dead(record, now_ms) {
                judged(record);
                remove_attached_if_unchanged(paths, record);
            }
            there
        })
        .collect()
}

/// The app a client of this Mac's tmux belongs to ([`app_client`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppClient {
    /// The app's lease holder: what its `left` line is addressed to.
    pub holder: String,
    /// The pane the client was in when it was asked, when tmux said.
    pub pane: Option<String>,
}

/// The one rule that tells a client the app started from a client a person attached by hand. For this
/// Mac, the client's parent process is a live app (the pid of a record of [`read_app_records`]) whose record
/// names this very client. For a machine, the client's terminal has a record under `attached/` that names this
/// very client process ([`machine_client_with`]), whichever app attached it and whoever holds the lease. A client that
/// is both is the app's own. `None` when no app runs and nothing is attached, which is asked first and costs
/// tmux nothing, or when the client is anyone else's.
pub fn app_client(paths: &Paths, tmux: &crate::tmux::Tmux, client: &str) -> Option<AppClient> {
    app_client_named(paths, tmux, Some(client)).map(|(_, app)| app)
}

/// [`app_client`] for the client the caller runs from (`#{client_name}`, as a key binding's `run-shell` resolves
/// it): its name comes back with the answer. Nothing is asked of tmux while no app runs and nothing is attached,
/// not even the client's name, so a verb that asks this first says to tmux what it always said.
pub fn current_app_client(paths: &Paths, tmux: &crate::tmux::Tmux) -> Option<(String, AppClient)> {
    app_client_named(paths, tmux, None)
}

/// The client the caller runs from, by name, and the app it belongs to when `ask_app` and one runs or a machine
/// attach is there (the rule of [`app_client`]), asked of tmux ONCE: with an app or an attach, the name, pid, pane
/// and terminal come in one `display-message`, and with none (or no `ask_app`) only the name, as always. The name
/// is empty when no client is attached. `Err` is tmux failing to answer.
pub fn current_client_and_app(
    paths: &Paths,
    tmux: &crate::tmux::Tmux,
    ask_app: bool,
) -> anyhow::Result<(String, Option<AppClient>)> {
    use anyhow::Context;
    // The call shape comes from a look that removes nothing: a record of a dead app is removed only once there is a
    // client, as it was before this verb asked about the app.
    if !(ask_app && an_app_runs(paths)) {
        // At most machine attaches are recorded: the name is asked first, as always, so that with no client nothing
        // else is asked and nothing is removed, and the records are read (dead ones removed, the attaches checked
        // against `list-clients`) only for a client that is there; when none of them is left, nothing more is asked.
        let name = tmux
            .run(&["display-message", "-p", "#{client_name}"])
            .context("finding the client the popup opens on")?;
        let name = name.trim().to_string();
        if name.is_empty() || !ask_app {
            return Ok((name, None));
        }
        let Some(known) = known_apps(paths) else {
            return Ok((name, None));
        };
        if known.records.is_empty() && known.attached.is_empty() {
            return Ok((name, None));
        }
        let known = known.settled(paths, tmux);
        if known.records.is_empty() && known.attached.is_empty() {
            return Ok((name, None));
        }
        let (_, said) =
            ask_client(tmux, Some(&name)).context("finding the client the popup opens on")?;
        return Ok((name, said.and_then(|said| app_client_in(known, said))));
    }
    // An app runs: the name and the rest come in one call. With no client tmux says no pid, so there is nothing to
    // look up and no record is read, checked or removed.
    let (name, said) = ask_client(tmux, None).context("finding the client the popup opens on")?;
    if name.is_empty() {
        return Ok((name, None));
    }
    let app = known_apps(paths)
        .zip(said)
        .and_then(|(known, said)| app_client_in(known.settled(paths, tmux), said));
    Ok((name, app))
}

/// Whether an app that is alive has a record, looked at without changing anything (`read_app_records` removes the
/// record of an app that was killed).
fn an_app_runs(paths: &Paths) -> bool {
    std::fs::read_dir(paths.apps_dir()).is_ok_and(|dir| {
        dir.flatten().any(|entry| {
            let name = entry.file_name().into_string().ok();
            let pid = name
                .as_deref()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|pid| pid.parse::<u32>().ok());
            pid.is_some_and(|pid| read_app_record(paths, pid).is_some() && app_alive(pid))
        })
    })
}

/// What is known of the apps from the files alone: the records of the apps that run and the machine attaches.
/// The attaches are not yet checked against the clients tmux has ([`KnownApps::settled`]).
struct KnownApps {
    records: Vec<AppRecord>,
    attached: Vec<AttachedRecord>,
}

impl KnownApps {
    /// The same, with the attaches whose client is still there ([`attached_with_a_client`]): asked of tmux (and
    /// dead records removed) only now that there is a client to look up.
    fn settled(mut self, paths: &Paths, tmux: &crate::tmux::Tmux) -> KnownApps {
        if !self.attached.is_empty() {
            self.attached = attached_with_a_client(paths, tmux);
        }
        self
    }
}

/// The apps there are, `None` when no app runs and no attach is recorded: read from files, nothing asked of tmux.
fn known_apps(paths: &Paths) -> Option<KnownApps> {
    let records = read_app_records(paths);
    let attached = read_attached_records(paths);
    (!(records.is_empty() && attached.is_empty())).then_some(KnownApps { records, attached })
}

/// The format a client is asked in, one call for the four answers the rule needs.
const CLIENT_ASK: &str = "#{client_name}\t#{client_pid}\t#{pane_id}\t#{client_tty}";

/// The name of `client` (the current client when `None`) and what the rule needs of it, from one `display-message`.
/// `None` for the second when tmux says no process for the client. A client that is gone, or a call that fails, is an
/// `Err`.
fn ask_client(
    tmux: &crate::tmux::Tmux,
    client: Option<&str>,
) -> anyhow::Result<(String, Option<ClientSaid>)> {
    let mut args = vec!["display-message"];
    if let Some(client) = client {
        args.extend_from_slice(&["-c", client]);
    }
    args.extend_from_slice(&["-p", CLIENT_ASK]);
    let said = tmux.run(&args)?;
    Ok(parse_client_line(&said))
}

/// The name, and the rest, of what [`CLIENT_ASK`] says: four words split by tabs (a word may be empty).
fn parse_client_line(said: &str) -> (String, Option<ClientSaid>) {
    let said = said.trim_end_matches('\n');
    let Some((name, rest)) = said.split_once('\t') else {
        return (said.trim().to_string(), None);
    };
    (
        name.trim().to_string(),
        parse_client_said(&rest.replace('\t', " ")),
    )
}

/// The rule of [`app_client`], with the client asked of tmux once and only when an app or an attached record exists.
fn app_client_named(
    paths: &Paths,
    tmux: &crate::tmux::Tmux,
    client: Option<&str>,
) -> Option<(String, AppClient)> {
    let known = known_apps(paths)?;
    let (name, said) = ask_client(tmux, client).ok()?;
    let client = client.map_or(name, str::to_string);
    if client.is_empty() {
        return None;
    }
    app_client_in(known.settled(paths, tmux), said?).map(|app| (client, app))
}

/// The app `said` (a client's pid, pane and terminal) belongs to, by the records of `known`.
fn app_client_in(known: KnownApps, said: ClientSaid) -> Option<AppClient> {
    if !known.records.is_empty()
        && let Some(parent) = parent_pid(said.pid)
        && let Some(app) = app_client_with(&known.records, said.pid, parent, said.pane.clone())
    {
        return Some(app);
    }
    let record = known
        .attached
        .iter()
        .find(|record| record.tty == said.tty)?;
    machine_client_with(record, said.pid, said.pane)
}

/// What `#{client_pid} #{pane_id} #{client_tty}` says of a client.
struct ClientSaid {
    pid: u32,
    pane: Option<String>,
    /// Empty when tmux said none.
    tty: String,
}

fn parse_client_said(said: &str) -> Option<ClientSaid> {
    // By position, since a word may be empty (a client with no pane says none).
    let mut words = said.trim_end().split(' ');
    let pid = words.next()?.parse().ok().filter(|pid| *pid != 0)?;
    let pane = words.next().filter(|pane| pane.starts_with('%'));
    let tty = words.next().unwrap_or_default();
    Some(ClientSaid {
        pid,
        pane: pane.map(str::to_string),
        tty: tty.to_string(),
    })
}

/// The app of another machine that the client belongs to, by the `record` of the terminal it runs on: the app
/// that attached it, whichever it is (the app a person started, an app the popup's doorway started, a second
/// app, an app reopened a moment after the last one quit). The lease says which app draws this machine's
/// notifications and has no say here: an app that does not hold it (a doorway app does not keep the
/// connections, so it never offers one) still gets its own client back. The client is the app's for as long as
/// it runs, since the attach is the app's ssh and ends with the app. A record is for the client process it
/// names only (`client` is the one asked about); one that names none (pid 0) is for no client.
fn machine_client_with(
    record: &AttachedRecord,
    client: u32,
    pane: Option<String>,
) -> Option<AppClient> {
    (record.pid != 0 && record.pid == client).then(|| AppClient {
        holder: record.holder.clone(),
        pane,
    })
}

/// The app among `records` that is the process `parent`, for the client `client` whose parent that is: it
/// runs, and its record names this client (the child writes the attach's pid into it before it waits), so a
/// pid that was reused, or a client somebody attached by hand from the app's own shell, is no app's.
fn app_client_with(
    records: &[AppRecord],
    client: u32,
    parent: u32,
    pane: Option<String>,
) -> Option<AppClient> {
    records
        .iter()
        .find(|record| {
            record.pid != 0
                && record.pid == parent
                && record.client_pid == client
                && app_alive(record.pid)
        })
        .map(|record| AppClient {
            holder: record.holder.clone(),
            pane,
        })
}

/// The parent of the process `pid`, asked of the system: `proc_pidinfo` on macOS (`pbi_ppid`) and `/proc/<pid>/stat`
/// on Linux. A `ps` is a process started (and, on macOS, a setuid one) every time a popup opens while an app runs.
/// `None` when there is no such process, or when it cannot be said, which is no app. Where the system cannot be
/// asked (another system, or a failed call) it is `ps -o ppid= -p`, asked as the watcher asks `ps`.
fn parent_pid(pid: u32) -> Option<u32> {
    parent_pid_with(pid, parent_of, |args| crate::watch::printed_by("ps", args))
}

/// [`parent_pid`] with the system's answer and `ps` as given (the seam the tests stand in at).
fn parent_pid_with(
    pid: u32,
    system: impl FnOnce(u32) -> Parent,
    ps: impl FnOnce(&[&str]) -> Option<String>,
) -> Option<u32> {
    match system(pid) {
        Parent::Is(parent) => Some(parent),
        Parent::Gone => None,
        Parent::Failed => ps(&["-o", "ppid=", "-p", &pid.to_string()])?
            .trim()
            .parse()
            .ok(),
    }
}

/// What the system said of the parent of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    Is(u32),
    /// There is no such process.
    Gone,
    /// The system could not be asked, or did not answer: `ps` is to be.
    Failed,
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Parent {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        // No pid is that large: nothing runs as it.
        return Parent::Gone;
    };
    // SAFETY: an all-zero `proc_bsdinfo` is a valid value (integers and byte arrays only).
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).unwrap_or(0);
    // SAFETY: `info` is `size` bytes that can be written to and the call is told so, so the
    // kernel writes no more than that and returns how many bytes it wrote.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    // `errno` is read only for a call that wrote nothing: it is that call's own then.
    let errno = if wrote <= 0 {
        std::io::Error::last_os_error().raw_os_error()
    } else {
        None
    };
    parent_from_call(wrote, size, info.pbi_ppid, errno)
}

/// What `proc_pidinfo` said, as [`Parent`]: the whole `size` written is the answer; nothing written with `ESRCH` is
/// a pid that is gone; anything else (another error, or a short write, which leaves an old `errno` behind) is a
/// failure and `ps` is to be asked.
#[cfg(target_os = "macos")]
fn parent_from_call(
    wrote: libc::c_int,
    size: libc::c_int,
    ppid: u32,
    errno: Option<i32>,
) -> Parent {
    if wrote == size {
        Parent::Is(ppid)
    } else if wrote <= 0 && errno == Some(libc::ESRCH) {
        Parent::Gone
    } else {
        Parent::Failed
    }
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Parent {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat_ppid(&stat).map_or(Parent::Failed, Parent::Is),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Parent::Gone,
        Err(_) => Parent::Failed,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_of(_pid: u32) -> Parent {
    Parent::Failed
}

/// The parent of a `/proc/<pid>/stat` line: the field after the state. The second field is the command in
/// parentheses, and it can hold spaces and parentheses itself, so the fields are counted from the last `)`:
/// state, then ppid.
#[cfg(any(target_os = "linux", test))]
fn stat_ppid(stat: &str) -> Option<u32> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// The JSON object in `path`, or `None` for a file that is absent, empty, not JSON or not an object.
fn read_object<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = read_capped(path)?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if !value.is_object() {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Writes `value` whole: to a file beside `path` and then over it, so a reader never sees half of it.
fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let mut text = serde_json::to_string(value).map_err(std::io::Error::other)?;
    text.push('\n');
    crate::atomic_file::write_whole(path, &text, "file")
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &std::path::Path) -> Paths {
        Paths::rooted_at(dir.join("config"), dir.join("state"))
    }

    fn write(paths: &Paths, name: &str, text: &str) {
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        std::fs::write(paths.machine_file(name).unwrap(), text).unwrap();
    }

    #[test]
    fn a_file_reads_with_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            &paths,
            "studio",
            r#"{"name":"studio","state":{"spaces":[{"session_id":"$1","session_name":"garden"}]},"received_at":5,"last_seen":7,"live":true,
                "words":"connected: taw 0.1.0","taw_version":"0.1.0","protocol":1,"this_server":true}"#,
        );
        let file = read(&paths, "studio").unwrap();
        assert_eq!(file.name, "studio");
        let state = file.state.expect("the state types");
        assert_eq!(state.spaces.len(), 1);
        assert_eq!(state.spaces[0].session_name, "garden");
        assert_eq!((file.received_at, file.last_seen), (Some(5), Some(7)));
        assert!(file.live && file.this_server);
        assert_eq!(file.words, "connected: taw 0.1.0");
        assert_eq!(file.taw_version.as_deref(), Some("0.1.0"));
        assert_eq!(file.protocol, Some(1));
    }

    #[test]
    fn missing_and_unknown_fields_read_as_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(&paths, "pi", "{}");
        assert_eq!(read(&paths, "pi"), Some(MachineFile::default()));
        write(&paths, "pi", r#"{"words":"x","later":[1,{"y":null}]}"#);
        let file = read(&paths, "pi").unwrap();
        assert_eq!(file.words, "x");
        assert_eq!(file.last_seen, None);
    }

    #[test]
    fn a_file_that_is_absent_empty_or_not_json_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(read(&paths, "studio"), None);
        for text in ["", "not json", "[]", "{\"last_seen\":\"x\"}", "null"] {
            write(&paths, "studio", text);
            assert_eq!(read(&paths, "studio"), None, "{text:?}");
        }
    }

    #[test]
    fn a_file_that_names_another_machine_is_none_and_one_with_no_name_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(&paths, "studio", r#"{"name":"pi","words":"pi's words"}"#);
        assert_eq!(read(&paths, "studio"), None);
        write(&paths, "studio", r#"{"words":"no name"}"#);
        assert_eq!(read(&paths, "studio").unwrap().words, "no name");
        write(&paths, "studio", r#"{"name":"studio","words":"its own"}"#);
        assert_eq!(read(&paths, "studio").unwrap().words, "its own");
    }

    #[test]
    fn a_name_that_is_not_a_plain_file_name_reads_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(&paths, "studio", "{}");
        std::fs::write(paths.state_dir.join("secret.json"), "{}").unwrap();
        for name in ["", ".", "..", "../secret", "a/b", ".hidden"] {
            assert_eq!(read(&paths, name), None, "{name:?}");
        }
    }

    /// `read` of `name` on a thread, so that a read that blocks fails the test instead of hanging it.
    fn read_within_2s(paths: &Paths, name: &str) -> Option<MachineFile> {
        let (send, receive) = std::sync::mpsc::channel();
        let (paths, name) = (paths.clone(), name.to_string());
        std::thread::spawn(move || {
            let _ = send.send(read(&paths, &name));
        });
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("read did not return within 2 seconds")
    }

    #[test]
    fn a_fifo_is_none_and_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        let path = std::ffi::CString::new(
            paths
                .machine_file("studio")
                .unwrap()
                .as_os_str()
                .to_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert_eq!(read_within_2s(&paths, "studio"), None);
    }

    #[test]
    fn a_fifo_that_holds_a_good_file_is_none_too() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        let file = paths.machine_file("studio").unwrap();
        let c_path = std::ffi::CString::new(file.as_os_str().to_str().unwrap().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // A reader that stays open keeps what a writer leaves in the pipe after it has gone, so that
        // what is read next is a whole file's text, then the end: only a look at what the path is
        // keeps it from being taken for one.
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&file)
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .write_all(br#"{"name":"studio","taw_version":"0.3.0"}"#)
            .unwrap();
        assert_eq!(read_within_2s(&paths, "studio"), None);
    }

    #[test]
    fn a_directory_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.machine_file("studio").unwrap()).unwrap();
        assert_eq!(read_within_2s(&paths, "studio"), None);
    }

    #[test]
    fn a_file_over_the_cap_is_none_and_one_at_the_cap_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let with_length = |length: u64| {
            let (head, tail) = (r#"{"words":""#, r#""}"#);
            let pad = length as usize - head.len() - tail.len();
            format!("{head}{}{tail}", "x".repeat(pad))
        };
        write(&paths, "studio", &with_length(READ_CAP + 1));
        assert_eq!(read(&paths, "studio"), None);
        write(&paths, "studio", &with_length(READ_CAP));
        assert_eq!(
            read(&paths, "studio").unwrap().words.len() as u64,
            READ_CAP - 12
        );
    }

    #[test]
    fn a_link_to_a_good_file_still_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        let target = tmp.path().join("elsewhere.json");
        std::fs::write(&target, r#"{"words":"linked"}"#).unwrap();
        std::os::unix::fs::symlink(&target, paths.machine_file("studio").unwrap()).unwrap();
        assert_eq!(read(&paths, "studio").unwrap().words, "linked");
    }

    #[test]
    fn live_needs_the_keeper_the_files_word_and_a_last_seen_within_15_seconds() {
        let file = |live, last_seen| MachineFile {
            live,
            last_seen,
            ..MachineFile::default()
        };
        let now = 1_790_000_100_000;
        assert!(is_live(&file(true, Some(now)), true, now));
        assert!(is_live(&file(true, Some(now - 15_000)), true, now));
        assert!(!is_live(&file(true, Some(now - 15_001)), true, now));
        assert!(!is_live(&file(true, None), true, now));
        assert!(!is_live(&file(false, Some(now)), true, now));
        assert!(!is_live(&file(true, Some(now)), false, now));
        // A clock that is ahead by more than 15 s is no better than one that is behind.
        assert!(!is_live(&file(true, Some(now + 15_001)), true, now));
    }

    #[test]
    fn the_words_show_only_while_a_keeper_keeps_the_file() {
        let file = MachineFile {
            words: "connected: taw 0.1.0".to_string(),
            ..MachineFile::default()
        };
        assert_eq!(words_shown(&file, true), "connected: taw 0.1.0");
        assert_eq!(words_shown(&file, false), "");
    }

    #[test]
    fn a_state_that_does_not_type_reads_as_none_and_the_rest_of_the_file_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for state in [
            r#""state":"not a state""#,
            r#""state":[1,2]"#,
            r#""state":{"spaces":"x"}"#,
            r#""state":{"panes":[{"window_index":"two"}]}"#,
            r#""state":null"#,
            r#""state":7"#,
        ] {
            write(
                &paths,
                "studio",
                &format!(r#"{{"name":"studio",{state},"last_seen":7,"live":true,"words":"w"}}"#),
            );
            let file = read(&paths, "studio").unwrap_or_else(|| panic!("{state} lost the file"));
            assert_eq!(file.state, None, "{state}");
            assert_eq!(
                (
                    file.name.as_str(),
                    file.last_seen,
                    file.live,
                    file.words.as_str()
                ),
                ("studio", Some(7), true, "w"),
                "{state}"
            );
        }
    }

    #[test]
    fn a_file_with_no_state_field_reads_with_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(&paths, "pi", r#"{"words":"x"}"#);
        assert_eq!(read(&paths, "pi").unwrap().state, None);
    }

    #[test]
    fn a_written_file_reads_back_with_its_state_and_leaves_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let file = MachineFile {
            name: "studio".to_string(),
            state: Some(wire::State {
                spaces: vec![wire::WireSpace {
                    session_id: "$1".to_string(),
                    session_name: "garden".to_string(),
                    ..wire::WireSpace::default()
                }],
                ..wire::State::default()
            }),
            received_at: Some(5),
            last_seen: Some(7),
            live: true,
            words: "connected: taw 0.1.0".to_string(),
            short: String::new(),
            taw_version: Some("0.1.0".to_string()),
            protocol: Some(1),
            this_server: false,
        };
        write_machine_file(&paths, &file);
        assert_eq!(read(&paths, "studio"), Some(file));
        let names: Vec<String> = std::fs::read_dir(paths.machines_dir())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["studio.json"]);
    }

    #[test]
    fn the_short_form_of_steady_words_reaches_the_view_only_while_a_keeper_keeps_the_file() {
        let steady = MachineFile {
            name: "pi".to_string(),
            words: "no taw there: install it as you did here".to_string(),
            short: "no taw there: install it".to_string(),
            ..MachineFile::default()
        };
        let kept = view_of(Some(&steady), "pi", true, 0);
        assert_eq!(kept.short, "no taw there: install it");
        // A file nobody keeps says `connecting`, which changes: no short form.
        let left = view_of(Some(&steady), "pi", false, 0);
        assert!(left.words.starts_with("connecting"), "{:?}", left.words);
        assert_eq!(left.short, "");
        assert_eq!(view_of(None, "pi", true, 0).short, "");
    }

    #[test]
    fn a_file_for_a_name_that_is_not_a_machines_is_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for name in ["", "..", "../out", "a/b"] {
            write_machine_file(
                &paths,
                &MachineFile {
                    name: name.to_string(),
                    ..MachineFile::default()
                },
            );
        }
        assert!(!paths.machines_dir().exists());
        assert!(!tmp.path().join("out.json").exists());
    }

    #[test]
    fn no_app_keeps_the_files_until_one_takes_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!keeper_alive(&paths_in(tmp.path())));
    }

    #[test]
    fn looking_for_a_keeper_makes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert!(!keeper_alive(&paths));
        assert!(!paths.machines_dir().exists());
        // A folder with no lock file in it gets none.
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        assert!(!keeper_alive(&paths));
        assert!(!paths.keeper_lock_file().exists());
    }

    #[test]
    fn a_folder_where_the_lock_file_goes_is_no_keeper_and_cannot_be_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.keeper_lock_file()).unwrap();
        assert!(!keeper_alive(&paths));
        assert!(take_keeper(&paths).is_none());
        assert!(!keeper_alive(&paths));
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_is_no_keeper() {
        use std::os::unix::fs::PermissionsExt;
        if crate::test_root::skip_as_root(
            "root opens a file of mode 000, so the check would say nothing",
        ) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // Positive: an app holds the lock, and its file says so while it can be read.
        let held = take_keeper(&paths).expect("the first takes it");
        assert!(keeper_alive(&paths));
        // A keeper is there, but this process cannot open its file: it is not shown as a keeper, and
        // another take is no better (it cannot open the file either).
        std::fs::set_permissions(
            paths.keeper_lock_file(),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        assert!(!keeper_alive(&paths));
        assert!(take_keeper(&paths).is_none());
        drop(held);
    }

    #[test]
    fn the_lock_is_one_apps_until_it_lets_go_and_then_another_takes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let first = take_keeper(&paths).expect("the first takes it");
        assert!(keeper_alive(&paths));
        assert!(take_keeper(&paths).is_none());
        // Looking does not take it, and a look does not let go of it.
        assert!(keeper_alive(&paths));
        assert!(take_keeper(&paths).is_none());
        drop(first);
        // The lock goes with the last descriptor of the open file. A process another test is starting meanwhile
        // may hold one for the moments between its fork and its exec (the descriptor is close-on-exec, so no
        // longer), so the release is waited for, and not asserted at one instant.
        let lock_file = paths.keeper_lock_file();
        let free = |what: &str| {
            let until = std::time::Instant::now() + Duration::from_secs(20);
            while keeper_alive(&paths) {
                assert!(
                    std::time::Instant::now() < until,
                    "{what}: the lock is still held:\n{}",
                    holders_of(&lock_file)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        free("after the first let go");
        let second = take_keeper(&paths).expect("the second takes it once it is free");
        assert!(keeper_alive(&paths));
        drop(second);
        free("after the second let go");
    }

    /// A lock taken removes the holders' lock files older than five minutes that no process holds, and keeps an old
    /// one that is held and a young one that is not (a holder that has just gone, whose lease the next app takes at
    /// once).
    #[test]
    fn a_lock_taken_removes_old_holder_files_nobody_holds_and_keeps_held_or_young_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(tmp.path().join("config"), tmp.path().join("state"));
        let folder = paths.machines_dir().join("holders");
        std::fs::create_dir_all(&folder).unwrap();
        let made_ago = |path: &Path, ago: Duration| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - ago)
                .unwrap();
        };
        // Held first: a lock taken removes the old files, which come after it.
        let held = take_holder_lock(&paths, "mac-13").expect("a lock of its own");
        let held_file = folder.join("mac-13.lock");
        made_ago(&held_file, Duration::from_secs(10 * 60));
        let old = folder.join("mac-11.lock");
        std::fs::write(&old, b"").unwrap();
        made_ago(&old, Duration::from_secs(10 * 60));
        let young = folder.join("mac-12.lock");
        std::fs::write(&young, b"").unwrap();
        made_ago(&young, Duration::from_secs(60));
        assert!(
            old.exists(),
            "the positive half: the old file is there before"
        );

        let _taken = take_holder_lock(&paths, "mac-14").expect("the lock taken");
        assert!(!old.exists(), "an old file nobody holds goes");
        assert!(held_file.exists(), "a held file stays, however old");
        assert!(young.exists(), "a young file stays");
        assert!(folder.join("mac-14.lock").exists());
        assert!(!holder_gone(&paths, "mac-13"));
        drop(held);
    }

    /// The old file nobody holds of the holder the lease names (an app that ran more than five minutes and then
    /// crashed) stays when the next app takes its own lock, so that app takes the lease at once; once the lease
    /// names someone else, the next lock taken removes it.
    #[test]
    fn a_lock_taken_keeps_the_old_file_of_the_holder_the_lease_names() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(tmp.path().join("config"), tmp.path().join("state"));
        let folder = paths.machines_dir().join("holders");
        std::fs::create_dir_all(&folder).unwrap();
        let crashed = folder.join("mac-21.lock");
        std::fs::write(&crashed, b"").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&crashed)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(10 * 60))
            .unwrap();
        assert!(
            offer_lease(&paths, "mac-21", now_ms()),
            "the crashed app's lease"
        );

        let _next = take_holder_lock(&paths, "mac-22").expect("the next app's lock");
        assert!(
            crashed.exists(),
            "the file of the holder the lease names stays"
        );
        assert!(
            offer_lease(&paths, "mac-22", now_ms()),
            "the next app takes the fresh lease of a gone holder at once"
        );
        assert_eq!(read_lease(&paths).unwrap().holder, "mac-22");

        let _third = take_holder_lock(&paths, "mac-23").expect("a third app's lock");
        assert!(
            !crashed.exists(),
            "the old file goes once the lease names another holder"
        );
        assert!(folder.join("mac-22.lock").exists(), "a held file stays");
    }

    /// Who has `path` open, as `lsof` says (nothing when it cannot be run), for the message of a lock that stays held.
    fn holders_of(path: &Path) -> String {
        std::process::Command::new("lsof")
            .arg("--")
            .arg(path)
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default()
    }

    #[test]
    fn a_lock_file_that_is_a_fifo_is_not_waited_for_and_takes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        make_fifo(&paths.keeper_lock_file());
        let found = within_2s({
            let paths = paths.clone();
            move || (take_keeper(&paths).is_some(), keeper_alive(&paths))
        });
        assert_eq!(found, (false, false));
    }

    fn view_file(live: bool, last_seen: Option<u64>, words: &str) -> MachineFile {
        MachineFile {
            name: "studio".to_string(),
            live,
            last_seen,
            words: words.to_string(),
            this_server: true,
            ..MachineFile::default()
        }
    }

    #[test]
    fn a_machine_with_no_file_is_connecting_with_nothing_to_show() {
        let view = view_of(None, "studio", true, T0);
        assert_eq!(view.name, "studio");
        assert_eq!(view.words, "connecting");
        assert!(
            !view.live && view.state.is_none() && view.last_seen.is_none() && !view.this_server
        );
    }

    #[test]
    fn a_view_takes_the_files_words_and_liveness_only_while_a_keeper_keeps_it() {
        let file = view_file(true, Some(T0 - 2_000), "connected: taw 0.1.0");
        let kept = view_of(Some(&file), "studio", true, T0);
        assert!(kept.live && kept.this_server);
        assert_eq!(kept.words, "connected: taw 0.1.0");
        assert_eq!(kept.last_seen, Some(T0 - 2_000));
        // Nobody keeps it: its words would still say connected, and it is not live.
        let orphan = view_of(Some(&file), "studio", false, T0);
        assert!(!orphan.live);
        assert_eq!(orphan.words, "connecting, last seen 2s ago");
    }

    #[test]
    fn a_file_that_says_live_with_an_old_last_seen_is_not_live_in_the_view() {
        let file = view_file(true, Some(T0 - 16_000), "connected: taw 0.1.0");
        assert!(!view_of(Some(&file), "studio", true, T0).live);
        let none = view_file(true, None, "");
        let view = view_of(Some(&none), "studio", true, T0);
        assert!(!view.live);
        assert_eq!(view.words, "connecting");
    }

    const T0: u64 = 1_790_000_000_000;

    #[test]
    fn a_lease_is_taken_when_there_is_none_or_it_is_stale_and_kept_by_its_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(read_lease(&paths), None);
        assert!(offer_lease(&paths, "laptop-1", T0));
        assert_eq!(
            read_lease(&paths),
            Some(Lease {
                holder: "laptop-1".into(),
                refreshed_at: T0
            })
        );
        // Its own again refreshes it.
        assert!(offer_lease(&paths, "laptop-1", T0 + 5_000));
        assert_eq!(read_lease(&paths).unwrap().refreshed_at, T0 + 5_000);
        // Another holder's offer, while it is fresh, changes nothing.
        assert!(!offer_lease(&paths, "desktop-2", T0 + 10_000));
        let kept = read_lease(&paths).unwrap();
        assert_eq!(
            (kept.holder.as_str(), kept.refreshed_at),
            ("laptop-1", T0 + 5_000)
        );
        // Once it is stale, the other takes it.
        assert!(offer_lease(&paths, "desktop-2", T0 + 5_000 + 15_000));
        assert_eq!(read_lease(&paths).unwrap().holder, "desktop-2");
    }

    #[test]
    fn a_lease_is_fresh_for_under_fifteen_seconds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert!(!lease_fresh(&paths, T0));
        assert!(offer_lease(&paths, "laptop-1", T0));
        assert!(lease_fresh(&paths, T0));
        assert!(lease_fresh(&paths, T0 + 14_900));
        assert!(!lease_fresh(&paths, T0 + 15_000));
        assert!(!lease_fresh(&paths, T0 + 60_000));
        // A clock that is behind by as much is no better.
        assert!(lease_fresh(&paths, T0 - 14_900));
        assert!(!lease_fresh(&paths, T0 - 15_000));
    }

    /// A fresh lease whose holder is gone is taken at once; one whose holder is there is kept; a holder that
    /// lets go gives its lease up and leaves no lock file.
    #[test]
    fn a_fresh_lease_of_a_holder_that_is_gone_is_taken_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        // The holder is there: its lock is held, and its fresh lease is kept.
        let lock = take_holder_lock(&paths, "laptop-1").expect("the lock");
        assert!(offer_lease(&paths, "laptop-1", T0));
        assert!(!holder_gone(&paths, "laptop-1"));
        assert!(!offer_lease(&paths, "desktop-2", T0 + 1_000));
        assert_eq!(read_lease(&paths).unwrap().holder, "laptop-1");
        // A second taker of the same holder does not get the lock while it is held.
        assert!(take_holder_lock(&paths, "laptop-1").is_none());
        drop(lock);
        // Ended as a killed process ends: the lease stays, fresh, and so does the lock file, held by nobody.
        assert!(offer_lease(&paths, "laptop-1", T0));
        let path = holder_lock_path(&paths, "laptop-1").unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(holder_gone(&paths, "laptop-1"));
        assert!(offer_lease(&paths, "desktop-2", T0 + 2_000));
        assert_eq!(read_lease(&paths).unwrap().holder, "desktop-2");

        // A holder that lets go cleanly gives its own lease up and removes its file; another's lease stays.
        let lock = take_holder_lock(&paths, "desktop-2").expect("the lock");
        drop(lock);
        assert_eq!(read_lease(&paths), None);
        assert!(!holder_lock_path(&paths, "desktop-2").unwrap().exists());
        assert!(
            !holder_gone(&paths, "desktop-2"),
            "no file: not known to be gone"
        );
        assert!(offer_lease(&paths, "laptop-1", T0));
        drop(take_holder_lock(&paths, "desktop-2").expect("the lock"));
        assert_eq!(read_lease(&paths).unwrap().holder, "laptop-1");
    }

    #[test]
    fn a_lease_file_that_is_not_an_object_counts_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        for text in ["", "nope", "[]", "null", "{\"refreshed_at\":\"x\"}"] {
            std::fs::write(paths.lease_file(), text).unwrap();
            assert_eq!(read_lease(&paths), None, "{text:?}");
        }
        std::fs::write(paths.lease_file(), "{}").unwrap();
        assert_eq!(read_lease(&paths), Some(Lease::default()));
    }

    #[test]
    fn the_walk_is_written_whole_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(read_walk(&paths, "mac-7"), None);
        let stops = vec!["%7".to_string(), "studio:%16".to_string()];
        write_walk(&paths, "mac-7", "studio", &stops, T0);
        assert_eq!(
            read_walk(&paths, "mac-7"),
            Some(Walk {
                you: "studio".into(),
                stops: stops.clone(),
                at: T0,
                holder: "mac-7".into(),
            })
        );
        // The file is exactly this: the key on a machine reads these four names.
        let text = std::fs::read_to_string(walk_path(&paths, "mac-7").unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "you": "studio",
                "stops": ["%7", "studio:%16"],
                "at": T0,
                "holder": "mac-7"
            })
        );
        write_walk(&paths, "mac-7", "pi", &[], T0 + 1);
        assert_eq!(
            read_walk(&paths, "mac-7").unwrap().stops,
            Vec::<String>::new()
        );
        let leftovers = std::fs::read_dir(paths.walks_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    /// The walks of two holders live side by side and each reads its own, whichever wrote last.
    #[test]
    fn the_walks_of_two_holders_coexist_and_each_reads_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write_walk(&paths, "mac-7", "studio", &["%1".to_string()], T0);
        write_walk(&paths, "pi-9", "studio", &["%2".to_string()], T0 + 1);
        assert_eq!(read_walk(&paths, "mac-7").unwrap().stops, ["%1"]);
        assert_eq!(read_walk(&paths, "pi-9").unwrap().stops, ["%2"]);
        assert_eq!(read_walk(&paths, "mac-8"), None);
        assert_ne!(walk_path(&paths, "mac-7"), walk_path(&paths, "pi-9"));
    }

    /// Two holders whose names are alike to the file name share a file, and the file names its writer: the other
    /// holder reads nothing from it (the positive half is the writer reading its own).
    #[test]
    fn a_safe_name_collision_never_serves_the_other_holders_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(walk_path(&paths, "a/b"), walk_path(&paths, "a:b"));
        assert_eq!(
            walk_path(&paths, "a/b").unwrap().file_name().unwrap(),
            "a_b.json"
        );
        write_walk(&paths, "a/b", "studio", &["%1".to_string()], T0);
        assert_eq!(read_walk(&paths, "a/b").unwrap().stops, ["%1"]);
        assert_eq!(read_walk(&paths, "a:b"), None);
    }

    /// An empty holder (and one that is only dots, a folder's name) names no file: the writer writes nothing and the
    /// reader reads nothing, even with a file in the folder.
    #[test]
    fn an_empty_holder_is_refused_by_the_reader_and_the_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for holder in ["", ".", ".."] {
            write_walk(&paths, holder, "studio", &["%1".to_string()], T0);
            assert_eq!(walk_path(&paths, holder), None, "{holder:?}");
        }
        assert!(!paths.walks_dir().exists(), "the writer wrote something");
        // Positive half: a holder with a name writes, and the empty one still reads nothing next to it.
        write_walk(&paths, "mac-7", "studio", &["%1".to_string()], T0);
        assert!(read_walk(&paths, "mac-7").is_some());
        assert_eq!(read_walk(&paths, ""), None);
    }

    /// A write removes the other holders' files nobody has written for five minutes and keeps the young ones.
    #[test]
    fn a_walk_write_removes_old_files_of_other_holders_and_keeps_young_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for holder in ["old-1", "young-2", "mine-3"] {
            write_walk(&paths, holder, "studio", &[], T0);
        }
        let age = |holder: &str, secs: u64| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(walk_path(&paths, holder).unwrap())
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
                .unwrap();
        };
        age("old-1", 301);
        age("young-2", 200);
        age("mine-3", 301);

        write_walk(&paths, "mine-3", "studio", &["%1".to_string()], T0 + 1);

        assert_eq!(read_walk(&paths, "old-1"), None, "an old file stays");
        assert!(read_walk(&paths, "young-2").is_some(), "a young file went");
        assert!(read_walk(&paths, "mine-3").is_some());
    }

    /// The cleanup never removes the file it is told to keep, however old that is: the writer's own file is fresh
    /// when it runs, so only a call of its own can show it (the positive half is the other old file going).
    #[test]
    fn the_cleanup_keeps_the_file_it_is_told_to_keep_however_old() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // Both are written first: a write cleans up after itself, so the second would remove the first once it was old.
        for holder in ["kept-1", "gone-2"] {
            write_walk(&paths, holder, "studio", &[], T0);
        }
        for holder in ["kept-1", "gone-2"] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(walk_path(&paths, holder).unwrap())
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(301))
                .unwrap();
        }

        remove_old_walks(&paths, &walk_path(&paths, "kept-1").unwrap());

        assert_eq!(read_walk(&paths, "gone-2"), None, "an old file stays");
        assert!(read_walk(&paths, "kept-1").is_some(), "the kept file went");
    }

    /// A walk file written by hand without the holder reads as nothing for every holder: its writer is no one.
    #[test]
    fn a_walk_file_without_a_holder_is_no_ones_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let path = walk_path(&paths, "mac-7").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"you":"studio","stops":["%7"],"at":5}"#).unwrap();
        assert_eq!(read_walk(&paths, "mac-7"), None);
        // Positive half: the same file with the holder is read.
        std::fs::write(
            &path,
            r#"{"you":"studio","stops":["%7"],"at":5,"holder":"mac-7"}"#,
        )
        .unwrap();
        assert!(read_walk(&paths, "mac-7").is_some());
    }

    fn toast(text: &str) -> wire::Down {
        wire::Down::Toast(wire::Toast {
            text: text.to_string(),
            ..wire::Toast::default()
        })
    }

    #[test]
    fn events_appended_by_two_threads_are_whole_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mut tail = EventTail::at_end(&paths);
        std::thread::scope(|scope| {
            for who in ["a", "b"] {
                let paths = &paths;
                scope.spawn(move || {
                    for n in 0..200 {
                        // Long enough that a torn write would show.
                        append_event(paths, &toast(&format!("{who}{n} {}", "x".repeat(300))));
                    }
                });
            }
        });
        let text = std::fs::read_to_string(paths.events_file()).unwrap();
        assert_eq!(text.lines().count(), 400);
        for line in text.lines() {
            assert!(
                matches!(wire::parse_down(line), Ok(wire::Down::Toast(_))),
                "{line}"
            );
        }
        let heard = tail.read_new();
        assert_eq!(heard.len(), 400);
        // Each writer's own lines came in order.
        for who in ["a", "b"] {
            let own: Vec<usize> = heard
                .iter()
                .filter_map(|line| match line {
                    wire::Down::Toast(t) if t.text.starts_with(who) => {
                        t.text[1..].split(' ').next().and_then(|n| n.parse().ok())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(own, (0..200).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_tail_reads_only_what_was_written_after_it_started_and_only_whole_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        append_event(&paths, &toast("before"));
        let mut tail = EventTail::at_end(&paths);
        assert!(tail.read_new().is_empty());
        append_event(&paths, &toast("one"));
        append_event(&paths, &wire::Down::Ping);
        assert_eq!(tail.read_new(), vec![toast("one"), wire::Down::Ping]);
        assert!(tail.read_new().is_empty());
        // A line half written is left until its end arrives; a line that is not the feed's is skipped.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(paths.events_file())
            .unwrap();
        file.write_all(b"not json\n{\"type\":\"later\"}\n{\"type\":\"toast\",\"te")
            .unwrap();
        assert!(tail.read_new().is_empty());
        file.write_all(b"xt\":\"two\"}\n").unwrap();
        assert_eq!(tail.read_new(), vec![toast("two")]);
    }

    /// An events file of `len` bytes of whole toast lines, last written at `written`.
    fn events_file_of(paths: &Paths, len: u64, written: std::time::SystemTime) {
        std::fs::create_dir_all(paths.machines_dir()).unwrap();
        let line = wire::render(&toast(&"y".repeat(900)));
        let mut text = String::new();
        while (text.len() as u64) < len {
            text.push_str(&line);
        }
        std::fs::write(paths.events_file(), text).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(paths.events_file())
            .unwrap()
            .set_modified(written)
            .unwrap();
    }

    fn at(now_ms: u64) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_millis(now_ms)
    }

    fn events_len(paths: &Paths) -> u64 {
        std::fs::metadata(paths.events_file()).unwrap().len()
    }

    #[test]
    fn past_the_cap_but_written_a_moment_ago_events_are_kept_for_a_reader_mid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let now = now_ms();
        events_file_of(&paths, EVENTS_CAP + 10, at(now));
        let before = events_len(&paths);
        let mut tail = EventTail::at_end(&paths);
        append_event_at(&paths, &toast("X"), now + 100);
        append_event_at(&paths, &toast("Y"), now + 200);
        assert!(events_len(&paths) > before);
        assert_eq!(tail.read_new(), vec![toast("X"), toast("Y")]);
    }

    #[test]
    fn past_the_cap_and_quiet_for_two_seconds_the_file_is_emptied_and_the_tail_notices() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let now = now_ms();
        events_file_of(&paths, EVENTS_CAP + 10, at(now - 2_000));
        let mut tail = EventTail::at_end(&paths);
        append_event_at(&paths, &toast("after the cap"), now);
        assert!(events_len(&paths) < 1000);
        assert_eq!(tail.read_new(), vec![toast("after the cap")]);
        append_event_at(&paths, &toast("next"), now + 10);
        assert_eq!(tail.read_new(), vec![toast("next")]);
        // Written 1.9 s ago is not quiet enough.
        events_file_of(&paths, EVENTS_CAP + 10, at(now - 1_900));
        let before = events_len(&paths);
        append_event_at(&paths, &toast("kept"), now);
        assert!(events_len(&paths) > before);
    }

    #[test]
    fn past_one_mebibyte_the_file_is_emptied_whoever_is_mid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let now = now_ms();
        events_file_of(&paths, EVENTS_HARD_CAP + 10, at(now));
        let mut tail = EventTail::at_end(&paths);
        append_event_at(&paths, &toast("fresh"), now + 10);
        assert!(events_len(&paths) < 1000);
        assert_eq!(tail.read_new(), vec![toast("fresh")]);
    }

    /// `work` on a thread, so that one that blocks fails the test instead of hanging it.
    fn within_2s<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(work());
        });
        receive
            .recv_timeout(Duration::from_secs(2))
            .expect("did not return within 2 seconds")
    }

    fn make_fifo(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c_path = std::ffi::CString::new(path.as_os_str().to_str().unwrap().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn a_fifo_at_the_lease_or_the_walk_is_none_and_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_fifo(&paths.lease_file());
        make_fifo(&walk_path(&paths, "mac-7").unwrap());
        let (lease, walk) = (paths.clone(), paths);
        assert_eq!(within_2s(move || read_lease(&lease)), None);
        assert_eq!(within_2s(move || read_walk(&walk, "mac-7")), None);
    }

    #[test]
    fn a_fifo_at_the_events_file_is_read_as_nothing_and_not_waited_for() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_fifo(&paths.events_file());
        let mut tail = EventTail::at_end(&paths);
        assert_eq!(within_2s(move || tail.read_new()), Vec::new());
    }

    #[test]
    fn an_append_to_a_fifo_gives_up_and_writes_nothing_into_it() {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_fifo(&paths.events_file());
        // Nobody at the other end: opening it to write must not wait for one.
        let bare = paths.clone();
        within_2s(move || append_event(&bare, &toast("nobody reads")));
        // A reader at the other end: the open works, and what is there is not a file, so nothing goes in.
        let mut reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(paths.events_file())
            .unwrap();
        let held = paths.clone();
        within_2s(move || append_event(&held, &toast("into a pipe")));
        let mut got = Vec::new();
        let read = reader.read_to_end(&mut got);
        assert!(
            got.is_empty(),
            "{read:?}: {}",
            String::from_utf8_lossy(&got)
        );
    }

    #[test]
    fn the_append_itself_refuses_a_file_that_is_not_a_regular_one() {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        make_fifo(&paths.events_file());
        // A reader at the other end, so that the open works and only the look at what was opened can
        // refuse (the public path only logs the error, and on macOS a lock on a FIFO fails as well).
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(paths.events_file())
            .unwrap();
        let path = paths.events_file();
        let err = within_2s(move || append_to(&path, "into a pipe\n", 0)).unwrap_err();
        assert_eq!(err.to_string(), "not a regular file");
    }

    #[test]
    fn a_new_events_file_is_private_to_its_user() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        append_event(&paths, &toast("title and body of an agent"));
        let mode = std::fs::metadata(paths.events_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_write_that_fails_leaves_no_temporary_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // A directory where the file goes: the rename over it fails after the temporary file is written.
        std::fs::create_dir_all(paths.lease_file()).unwrap();
        assert!(!offer_lease(&paths, "laptop-1", T0));
        write_walk(&paths, "mac-7", "studio", &[], T0);
        let names: Vec<String> = std::fs::read_dir(paths.machines_dir())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name == "lease.json"), "{names:?}");
        assert!(names.iter().any(|name| name == "walks"), "{names:?}");
        assert!(
            !names.iter().any(|name| name.ends_with(".tmp")),
            "{names:?}"
        );
        assert!(
            read_walk(&paths, "mac-7").is_some(),
            "the walk was not written"
        );
    }

    fn record(pid: u32) -> AppRecord {
        AppRecord {
            pid,
            supervisor: 41,
            client_pid: 0,
            client_tty: String::new(),
            inside: String::new(),
            holder: format!("laptop-{pid}"),
            started_at: T0,
        }
    }

    #[test]
    fn only_a_real_process_id_is_an_app_that_is_alive() {
        assert!(app_alive(std::process::id()));
        assert!(!app_alive(0), "no pid");
        assert!(!app_alive(1), "not an app");
        assert!(!app_alive(i32::MAX as u32 + 1), "above any pid");
        assert!(!app_alive(u32::MAX), "above any pid");
    }

    #[test]
    fn a_record_named_for_a_pid_above_any_pid_is_not_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mut huge = record(u32::MAX);
        huge.supervisor = std::process::id();
        write_app_record(&paths, &huge);
        assert!(read_app_records(&paths).is_empty());
    }

    #[test]
    fn an_app_record_is_written_whole_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let mut mine = record(std::process::id());
        mine.client_pid = 777;
        mine.client_tty = "/dev/ttys004".to_string();
        mine.inside = "studio".to_string();
        write_app_record(&paths, &mine);
        assert!(paths.app_record(mine.pid).is_file());
        assert_eq!(read_app_record(&paths, mine.pid), Some(mine.clone()));
        assert_eq!(read_app_records(&paths), vec![mine]);
    }

    #[test]
    fn an_app_record_with_missing_fields_reads_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let pid = std::process::id();
        std::fs::create_dir_all(paths.apps_dir()).unwrap();
        std::fs::write(
            paths.app_record(pid),
            format!(r#"{{"pid":{pid},"later":[1,{{"x":null}}]}}"#),
        )
        .unwrap();
        let read = read_app_records(&paths);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].pid, pid);
        assert_eq!((read[0].supervisor, read[0].client_pid), (0, 0));
        assert_eq!(read[0].holder, "");
    }

    /// The pid of a process that has ended.
    fn a_dead_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        dead
    }

    #[test]
    fn a_record_whose_process_and_supervisor_are_gone_is_removed_as_it_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let dead = a_dead_pid();
        let mut both_gone = record(dead);
        both_gone.supervisor = a_dead_pid();
        write_app_record(&paths, &both_gone);
        // Both alive: this process is the app and its own supervisor.
        let mut alive = record(std::process::id());
        alive.supervisor = std::process::id();
        write_app_record(&paths, &alive);
        // The reader of one record looks only: the supervisor reads its dead child's.
        assert_eq!(read_app_record(&paths, dead).map(|r| r.pid), Some(dead));
        let read = read_app_records(&paths);
        assert_eq!(
            read.iter().map(|r| r.pid).collect::<Vec<_>>(),
            vec![std::process::id()]
        );
        assert!(!paths.app_record(dead).exists());
        assert!(paths.app_record(std::process::id()).exists());
    }

    /// The supervisor reads the record of the child that just died to find the client it must wait for, so
    /// until it removes the record itself, nobody else does.
    #[test]
    fn a_record_whose_process_is_gone_but_whose_supervisor_is_alive_is_left_for_the_supervisor() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let dead = a_dead_pid();
        let mut orphaned = record(dead);
        orphaned.supervisor = std::process::id();
        orphaned.client_pid = 4242;
        write_app_record(&paths, &orphaned);
        assert!(
            read_app_records(&paths).is_empty(),
            "a dead app is not one that runs"
        );
        assert!(paths.app_record(dead).exists(), "its supervisor removes it");
        assert_eq!(read_app_record(&paths, dead), Some(orphaned));
    }

    #[test]
    fn records_that_are_not_records_are_skipped_and_nothing_is_made_by_looking() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert!(read_app_records(&paths).is_empty());
        assert!(!paths.apps_dir().exists(), "looking made the folder");
        std::fs::create_dir_all(paths.apps_dir()).unwrap();
        let me = std::process::id();
        // Not an object, not JSON, a name that is no pid, and a record under another pid's name.
        std::fs::write(paths.apps_dir().join("1.json"), "[1]").unwrap();
        std::fs::write(paths.apps_dir().join("2.json"), "nope").unwrap();
        std::fs::write(
            paths.apps_dir().join("x.json"),
            format!(r#"{{"pid":{me}}}"#),
        )
        .unwrap();
        // Under this process's name, but a record of a live process that is another's.
        let other = std::os::unix::process::parent_id();
        std::fs::write(
            paths.apps_dir().join(format!("{me}.json")),
            format!(r#"{{"pid":{other}}}"#),
        )
        .unwrap();
        std::fs::write(paths.apps_dir().join("note.txt"), "x").unwrap();
        assert!(read_app_records(&paths).is_empty());
    }

    #[test]
    fn an_app_record_is_removed_by_its_pid_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let me = std::process::id();
        write_app_record(&paths, &record(me));
        let mode = std::fs::metadata(paths.app_record(me))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        remove_app_record(&paths, me);
        assert!(read_app_records(&paths).is_empty());
        // Removing what is not there is no error.
        remove_app_record(&paths, me);
    }

    #[test]
    fn a_client_belongs_to_the_app_that_is_its_parent_and_names_it() {
        // Two live processes: this one and its parent, each an app that waits for client 4242 and 4343.
        let (me, other) = (std::process::id(), std::os::unix::process::parent_id());
        let records = [
            AppRecord {
                pid: me,
                client_pid: 4242,
                holder: "mac-me".into(),
                ..AppRecord::default()
            },
            AppRecord {
                pid: other,
                client_pid: 4343,
                holder: "mac-other".into(),
                ..AppRecord::default()
            },
        ];
        let got = app_client_with(&records, 4343, other, Some("%3".into())).unwrap();
        assert_eq!(got.holder, "mac-other");
        assert_eq!(got.pane.as_deref(), Some("%3"));
        assert_eq!(
            app_client_with(&records, 4242, me, None).unwrap().holder,
            "mac-me"
        );
        assert_eq!(app_client_with(&records, 4343, 60, None), None);
        // A parent of 0 is no app, even for a record whose pid never was written.
        let blank = [AppRecord::default()];
        assert_eq!(app_client_with(&blank, 0, 0, None), None);
    }

    /// A record whose pid is the client's parent but that waits for another client is not that client's app:
    /// the pid was reused, or a person attached by hand from the app's own shell.
    #[test]
    fn a_record_that_names_another_client_is_not_this_clients_app() {
        let me = std::process::id();
        let records = [AppRecord {
            pid: me,
            client_pid: 4242,
            holder: "mac-me".into(),
            ..AppRecord::default()
        }];
        assert!(
            app_client_with(&records, 4242, me, None).is_some(),
            "positive half"
        );
        assert_eq!(app_client_with(&records, 5555, me, None), None);
        let waiting_for_none = [AppRecord {
            client_pid: 0,
            ..records[0].clone()
        }];
        assert_eq!(app_client_with(&waiting_for_none, 5555, me, None), None);
    }

    /// A record of a pid that is dead, whatever its supervisor, is no client's app even when the pid is the
    /// client's parent.
    #[test]
    fn a_record_of_a_dead_pid_is_not_an_app() {
        let dead = a_dead_pid();
        let records = [AppRecord {
            pid: dead,
            client_pid: 4242,
            supervisor: std::process::id(),
            holder: "mac-dead".into(),
            ..AppRecord::default()
        }];
        assert_eq!(app_client_with(&records, 4242, dead, None), None);
    }

    #[test]
    fn what_tmux_says_of_a_client_is_its_pid_its_pane_and_its_terminal() {
        let said = |text| parse_client_said(text).map(|s| (s.pid, s.pane, s.tty));
        assert_eq!(
            said("4242 %7 /dev/ttys004\n"),
            Some((4242, Some("%7".to_string()), "/dev/ttys004".to_string()))
        );
        assert_eq!(
            said("4242 %7\n"),
            Some((4242, Some("%7".to_string()), String::new()))
        );
        assert_eq!(said("4242 \n"), Some((4242, None, String::new())));
        // A client with no pane still has its terminal in the third place.
        assert_eq!(
            said("4242  /dev/ttys004"),
            Some((4242, None, "/dev/ttys004".to_string()))
        );
        assert_eq!(said("0 %7"), None);
        assert_eq!(said(" \n"), None);
        assert_eq!(said("x %7"), None);
    }

    /// The client is asked of tmux once, name and the rest together, for the key rule's both doors.
    #[test]
    fn a_client_is_asked_of_tmux_once() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                client_pid: 4242,
                ..Stub::default()
            },
        );
        let mut mine = record(std::process::id());
        mine.client_pid = 4242;
        write_app_record(&paths, &mine);
        // The parent of 4242 is nobody's here, so the client is no app's; what is pinned is the call count.
        assert_eq!(current_app_client(&paths, &tmux), None);
        assert_eq!(calls(&log).len(), 1, "{:?}", calls(&log));
        assert_eq!(app_client(&paths, &tmux, "/dev/ttys001"), None);
        assert_eq!(calls(&log).len(), 2, "{:?}", calls(&log));
        let (name, said) = parse_client_line("/dev/ttys001\t4242\t%5\t/dev/ttys004\n");
        assert_eq!(name, "/dev/ttys001");
        assert_eq!(
            said.map(|said| (said.pid, said.pane, said.tty)),
            Some((4242, Some("%5".to_string()), "/dev/ttys004".to_string()))
        );
        let (name, said) = parse_client_line("/dev/ttys001\t0\t\t\n");
        assert_eq!((name.as_str(), said.is_none()), ("/dev/ttys001", true));
        assert_eq!(parse_client_line("").0, "");
    }

    /// With no client at all the popup's open stops at the empty name, as it always did: one call, and the attach
    /// records are neither checked against `list-clients` nor removed. With a client, the same records are checked.
    #[test]
    fn with_no_client_the_records_are_not_judged() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        // A record that is old and dead, which a check against `list-clients` would remove.
        let mut dead = attached_record("/dev/ttys042", "holder", 0);
        dead.at = 1_700_000_000_000;
        write_attached(&paths, &dead).unwrap();
        write_app_record(&paths, &record(std::process::id()));
        // Beside the live app, the record of one that was killed (its supervisor gone too), which reading the
        // records would remove.
        let killed_pid = a_dead_pid();
        let mut killed = record(killed_pid);
        killed.supervisor = a_dead_pid();
        write_app_record(&paths, &killed);
        let verbs = |log: &std::path::Path| -> Vec<String> {
            calls(log).iter().map(|call| call[0].clone()).collect()
        };

        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                clients: false,
                ..Stub::default()
            },
        );
        let (name, app) = current_client_and_app(&paths, &tmux, true).unwrap();
        assert_eq!((name.as_str(), app), ("", None));
        assert_eq!(verbs(&log), ["display-message"]);
        assert!(
            read_attached(&paths, "/dev/ttys042").is_some(),
            "judged and removed"
        );
        assert!(
            read_app_record(&paths, killed_pid).is_some(),
            "a killed app's record was removed with no client there"
        );

        // Positive half: with a client, `list-clients` is asked and the dead record goes.
        let again = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(
            again.path(),
            &Stub {
                client_pid: 4242,
                ..Stub::default()
            },
        );
        let (name, _) = current_client_and_app(&paths, &tmux, true).unwrap();
        assert_eq!(name, "/dev/ttys001");
        assert_eq!(verbs(&log), ["display-message", "list-clients"]);
        assert!(read_attached(&paths, "/dev/ttys042").is_none());
    }

    /// With no client, a record of an app that was killed (its process and its supervisor gone) is not removed on the
    /// way: the call shape comes from a look that removes nothing, and records are judged only for a client that is
    /// there, as before. The answer and the tmux calls are the old ones: one `display-message`, no `list-clients`.
    #[test]
    fn with_no_client_a_dead_app_record_is_left_where_it_is() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let dead = a_dead_pid();
        let mut killed = record(dead);
        killed.supervisor = a_dead_pid();
        write_app_record(&paths, &killed);
        let verbs = |log: &std::path::Path| -> Vec<String> {
            calls(log).iter().map(|call| call[0].clone()).collect()
        };
        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                clients: false,
                ..Stub::default()
            },
        );
        let (name, app) = current_client_and_app(&paths, &tmux, true).unwrap();
        assert_eq!((name.as_str(), app), ("", None));
        assert_eq!(verbs(&log), ["display-message"]);
        assert!(
            read_app_record(&paths, dead).is_some(),
            "the record was removed with no client there"
        );

        // Positive half: with a client the same record is judged and goes, as it always did.
        let again = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(again.path(), &Stub::default());
        let (name, _) = current_client_and_app(&paths, &tmux, true).unwrap();
        assert_eq!(name, "/dev/ttys001");
        assert!(
            read_app_record(&paths, dead).is_none(),
            "a client is there: the dead record goes"
        );
    }

    /// The same with machine attaches recorded and no app: with no client the name is the one call and the records
    /// are left alone; with a client, `list-clients` checks them (a dead one goes) and, as none is left, nothing
    /// more is asked.
    #[test]
    fn with_only_attaches_and_no_client_the_name_is_the_one_call() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        write_attached(&paths, &attached_record("/dev/ttys042", "holder", 0)).unwrap();
        let verbs = |log: &std::path::Path| -> Vec<String> {
            calls(log).iter().map(|call| call[0].clone()).collect()
        };
        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                clients: false,
                ..Stub::default()
            },
        );
        let (name, app) = current_client_and_app(&paths, &tmux, true).unwrap();
        assert_eq!((name.as_str(), app), ("", None));
        assert_eq!(verbs(&log), ["display-message"]);
        assert!(
            read_attached(&paths, "/dev/ttys042").is_some(),
            "judged and removed"
        );

        let again = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(again.path(), &Stub::default());
        let (name, app) = current_client_and_app(&paths, &tmux, true).unwrap();
        assert_eq!((name.as_str(), app), ("/dev/ttys001", None));
        assert_eq!(verbs(&log), ["display-message", "list-clients"]);
        assert!(read_attached(&paths, "/dev/ttys042").is_none());
    }

    /// The parent of a process is asked of the system: this process's own, a child's (this process), and none for
    /// a process that is gone.
    #[test]
    fn the_parent_of_a_process_comes_from_the_system() {
        let me = std::process::id();
        // SAFETY: `getppid` takes nothing and cannot fail.
        let mine = unsafe { libc::getppid() } as u32;
        assert_eq!(parent_pid(me), Some(mine));
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert_eq!(parent_pid(child.id()), Some(me));
        let pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        // A pid that is gone has no parent; one the system handed out again at once has another's, never ours.
        assert_ne!(parent_pid(pid), Some(me), "a pid that is gone");
        assert_eq!(parent_pid(u32::MAX - 1), None);
    }

    /// A short write is a failure whatever `errno` held from before; only a call that wrote nothing, with `ESRCH`,
    /// is a pid that went.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_short_write_of_the_system_is_a_failure_not_a_gone_pid() {
        let esrch = Some(libc::ESRCH);
        assert_eq!(parent_from_call(100, 100, 9, None), Parent::Is(9));
        assert_eq!(parent_from_call(0, 100, 0, esrch), Parent::Gone);
        assert_eq!(parent_from_call(-1, 100, 0, esrch), Parent::Gone);
        assert_eq!(parent_from_call(40, 100, 0, None), Parent::Failed);
        assert_eq!(parent_from_call(40, 100, 0, esrch), Parent::Failed);
        assert_eq!(
            parent_from_call(0, 100, 0, Some(libc::EPERM)),
            Parent::Failed
        );
    }

    /// `ps` is a fallback for a system that cannot be asked, and is not run otherwise.
    #[test]
    fn ps_is_asked_only_when_the_system_cannot_say() {
        let no_ps = |_: &[&str]| -> Option<String> { panic!("ps was run") };
        assert_eq!(parent_pid_with(7, |_| Parent::Is(3), no_ps), Some(3));
        assert_eq!(parent_pid_with(7, |_| Parent::Gone, no_ps), None);
        let ps = |args: &[&str]| {
            assert_eq!(args, ["-o", "ppid=", "-p", "7"]);
            Some("  412\n".to_string())
        };
        assert_eq!(parent_pid_with(7, |_| Parent::Failed, ps), Some(412));
        assert_eq!(parent_pid_with(7, |_| Parent::Failed, |_| None), None);
        assert_eq!(
            parent_pid_with(7, |_| Parent::Failed, |_| Some("junk".to_string())),
            None
        );
    }

    /// The parent in a `/proc/<pid>/stat` line, counted from the last parenthesis.
    #[test]
    fn the_parent_in_a_proc_stat_line_follows_a_command_with_parentheses() {
        assert_eq!(stat_ppid("812 (bash) S 811 812 812 34816 812"), Some(811));
        assert_eq!(stat_ppid("12 (a b) (c) R 1 12 12 0 -1"), Some(1));
        assert_eq!(stat_ppid("12 (short)"), None);
        assert_eq!(stat_ppid("no command"), None);
    }

    #[test]
    fn with_no_app_record_nothing_is_asked_of_tmux() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        // A stand-in that logs every call: a `None` alone would be the same if the ask failed.
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        assert_eq!(app_client(&paths, &tmux, "/dev/ttys001"), None);
        assert_eq!(calls(&log), Vec::<Vec<String>>::new(), "tmux was asked");

        // Positive half: with a record of a live app the same stand-in is asked for the client's pid.
        write_app_record(&paths, &record(std::process::id()));
        app_client(&paths, &tmux, "/dev/ttys001");
        assert!(
            calls(&log).iter().any(|call| call[0] == "display-message"
                && call.last().is_some_and(|w| w.contains("#{client_pid}"))),
            "{:?}",
            calls(&log)
        );
    }

    // ------------------------------------------------ the clients a machine attach left

    fn attached_record(tty: &str, holder: &str, pid: u32) -> AttachedRecord {
        AttachedRecord {
            tty: tty.into(),
            holder: holder.into(),
            at: 1_700_000_000_000,
            pid,
        }
    }

    #[test]
    fn an_attached_record_is_written_whole_privately_and_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        assert!(read_attached_records(&paths).is_empty());
        assert!(!paths.attached_dir().exists(), "looking made the folder");

        write_attached(&paths, &attached_record("/dev/ttys004", "mac-1", 77)).unwrap();
        write_attached(&paths, &attached_record("/dev/ttys002", "mac-2", 0)).unwrap();

        let file = paths.attached_dir().join("_dev_ttys004.json");
        let text = std::fs::read_to_string(&file).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["tty"], "/dev/ttys004");
        assert_eq!(json["holder"], "mac-1");
        assert_eq!(json["at"], 1_700_000_000_000u64);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            read_attached(&paths, "/dev/ttys004"),
            Some(attached_record("/dev/ttys004", "mac-1", 77))
        );
        let ttys: Vec<String> = read_attached_records(&paths)
            .into_iter()
            .map(|r| r.tty)
            .collect();
        assert_eq!(ttys, ["/dev/ttys002", "/dev/ttys004"]);
        remove_attached(&paths, "/dev/ttys004");
        assert_eq!(read_attached(&paths, "/dev/ttys004"), None);
        // Removing what is not there is no error.
        remove_attached(&paths, "/dev/ttys004");
        assert!(write_attached(&paths, &attached_record("", "mac-1", 0)).is_err());
    }

    /// Every field has a default: a record with only the terminal reads, one that is not an object or is
    /// another terminal's file does not count.
    #[test]
    fn an_attached_record_with_missing_fields_reads_and_a_stranger_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let folder = paths.attached_dir();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("_dev_ttys001.json"),
            r#"{"tty":"/dev/ttys001"}"#,
        )
        .unwrap();
        std::fs::write(folder.join("notes.json"), "not json").unwrap();
        std::fs::write(
            folder.join("_dev_ttys007.json"),
            r#"{"tty":"/dev/ttys008"}"#,
        )
        .unwrap();
        std::fs::write(folder.join("empty.json"), "").unwrap();

        let records = read_attached_records(&paths);

        assert_eq!(records, [attached_record_bare("/dev/ttys001")]);
        assert!(folder.join("notes.json").exists() && folder.join("_dev_ttys007.json").exists());
    }

    fn attached_record_bare(tty: &str) -> AttachedRecord {
        AttachedRecord {
            tty: tty.into(),
            ..AttachedRecord::default()
        }
    }

    /// The machine rule: the client the record names, whoever holds the lease (the rule takes none).
    #[test]
    fn a_machine_client_is_the_process_its_record_names() {
        let record = attached_record("/dev/ttys004", "mac-1", 4242);
        let got = machine_client_with(&record, 4242, Some("%3".into())).expect("positive half");
        assert_eq!(
            (got.holder.as_str(), got.pane.as_deref()),
            ("mac-1", Some("%3"))
        );
        // Another process holds the terminal; a record that names none (pid 0) matches no client.
        assert_eq!(machine_client_with(&record, 4243, Some("%3".into())), None);
        let nameless = attached_record("/dev/ttys004", "mac-1", 0);
        assert_eq!(machine_client_with(&nameless, 0, Some("%3".into())), None);
    }

    #[test]
    fn records_of_clients_that_are_gone_are_removed_as_they_are_read() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        // The stand-in lists one client, on /dev/ttys001.
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        write_attached(&paths, &attached_record("/dev/ttys001", "mac-1", 0)).unwrap();
        write_attached(&paths, &attached_record("/dev/ttys009", "mac-1", 0)).unwrap();

        let kept = attached_with_a_client(&paths, &tmux);

        assert!(calls(&log).iter().any(|call| call[0] == "list-clients"));
        assert_eq!(kept, [attached_record("/dev/ttys001", "mac-1", 0)]);
        assert!(
            read_attached(&paths, "/dev/ttys009").is_none(),
            "the record stayed"
        );
        assert!(read_attached(&paths, "/dev/ttys001").is_some());

        // A tmux that cannot say which clients there are judges none: nothing is removed.
        let broken = crate::tmux::Tmux {
            bin: dir.path().join("no-such-tmux"),
            socket: None,
        };
        write_attached(&paths, &attached_record("/dev/ttys009", "mac-1", 0)).unwrap();
        assert_eq!(attached_with_a_client(&paths, &broken).len(), 2);
        assert!(read_attached(&paths, "/dev/ttys009").is_some());
    }

    /// The stand-in lists one client, on /dev/ttys001; the record judged is of a terminal it does not list.
    fn judge(paths: &Paths, dir: &Path, now: u64, judged: &dyn Fn(&AttachedRecord)) -> Vec<String> {
        use crate::cli::test_support::{Stub, stub_tmux};
        let (tmux, _log) = stub_tmux(dir, &Stub::default());
        attached_with_a_client_at(paths, &tmux, now, judged)
            .into_iter()
            .map(|record| record.tty)
            .collect()
    }

    const GONE: &str = "/dev/ttys009";

    /// A process of the test's own that lives until it is ended, and is killed by its pid when dropped.
    struct Child(std::process::Child);

    impl Child {
        fn start() -> Child {
            Child(
                std::process::Command::new("/bin/sleep")
                    .arg("120")
                    .spawn()
                    .unwrap(),
            )
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A record whose terminal has no client yet is kept for 10 s (the client is on its way: the record is
    /// written before the exec), and a clock that went back keeps it too; the same record 11 s old with a
    /// dead pid is removed.
    #[test]
    fn a_fresh_record_with_no_client_on_its_terminal_stays_and_an_old_one_of_a_dead_pid_goes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let record = AttachedRecord {
            pid: a_dead_pid(),
            ..attached_record(GONE, "mac-1", 0)
        };
        let at = record.at;
        write_attached(&paths, &record).unwrap();
        // Only the stand-in's own client, on ttys001, is listed, and no record is for it.
        for now in [at, at + 1_000, at + 10_000, at - 5_000] {
            assert!(judge(&paths, dir.path(), now, &|_| {}).is_empty());
            assert_eq!(read_attached(&paths, GONE), Some(record.clone()), "{now}");
        }
        // Positive half: 11 s old, the same record goes.
        assert!(judge(&paths, dir.path(), at + 11_000, &|_| {}).is_empty());
        assert_eq!(read_attached(&paths, GONE), None);
    }

    /// A record whose pid is alive stays whatever its age and whatever the terminal says: that client is
    /// another server's, or has not shown on the list; once the process is gone the same record goes.
    #[test]
    fn an_old_record_of_a_live_pid_stays_even_with_no_client_on_its_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let mut client = Child::start();
        let record = attached_record(GONE, "mac-1", client.0.id());
        write_attached(&paths, &record).unwrap();
        let later = record.at + 3_600_000;

        assert!(judge(&paths, dir.path(), later, &|_| {}).is_empty());
        assert_eq!(read_attached(&paths, GONE), Some(record.clone()));

        // Positive half: the process ends (killed by its pid, which this test started) and the record goes.
        client.0.kill().unwrap();
        client.0.wait().unwrap();
        assert!(judge(&paths, dir.path(), later, &|_| {}).is_empty());
        assert_eq!(read_attached(&paths, GONE), None);
    }

    /// A record written for the terminal after the one judged was read is not removed: the file is read
    /// again before the removal and must be the record that was judged.
    #[test]
    fn a_record_replaced_between_the_judgement_and_the_removal_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let old = AttachedRecord {
            pid: a_dead_pid(),
            ..attached_record(GONE, "mac-1", 0)
        };
        write_attached(&paths, &old).unwrap();
        let newer = AttachedRecord {
            at: old.at + 11_500,
            pid: std::process::id(),
            ..old.clone()
        };
        let now = old.at + 12_000;

        let judged = std::cell::Cell::new(0);
        assert!(
            judge(&paths, dir.path(), now, &|record| {
                judged.set(judged.get() + 1);
                assert_eq!(record, &old);
                write_attached(&paths, &newer).unwrap();
            })
            .is_empty()
        );
        assert_eq!(judged.get(), 1, "the record was never judged dead");
        assert_eq!(read_attached(&paths, GONE), Some(newer.clone()));

        // Positive half: the record that was judged is removed when nothing replaced it.
        write_attached(&paths, &old).unwrap();
        judge(&paths, dir.path(), now, &|_| {});
        assert_eq!(read_attached(&paths, GONE), None);
        // One that went away in between is no error.
        write_attached(&paths, &old).unwrap();
        judge(&paths, dir.path(), now, &|record| {
            remove_attached(&paths, &record.tty)
        });
        assert_eq!(read_attached(&paths, GONE), None);
    }
}
