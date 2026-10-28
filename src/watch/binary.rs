//! Which taw a watcher runs, and whether that taw is still the one installed.
//!
//! tmux starts a watcher once and never restarts it, and a watcher lives as long as its pane.
//! So when taw is rebuilt or reinstalled (cargo replaces `target/release/taw` with a new file,
//! and the plugin's `bin/taw` is a link to that), every watcher keeps running the taw it was
//! started with, and nothing says so: a fix that is in the new taw does not reach the panes
//! that are already open. `taw watch status` marks those watchers and
//! `taw watch attach-all --restart` starts them again.
//!
//! A watcher cannot ask the system which file it was loaded from, so each writes down what it
//! runs as it starts, in the pane option [`OPTION`]:
//!
//! ```text
//! @agent_watch_build = "<pid> <device>:<inode>:<size>:<seconds>:<nanoseconds> <path>"
//! ```
//!
//! * **The path** is where the watcher was started from, as it was started: a link to a build
//!   is kept as the link, so a link moved to another build counts as a replacement (on Linux
//!   the system hands a process the path with links resolved, and only a replaced file
//!   counts). [`judge`] looks at that path again later, which is what makes "installed" mean
//!   "what is there now", whichever taw asks.
//! * **The identity** is the device, the inode, the size and the time of the last write of
//!   the file there. A build or an installer puts a new file where the old one was, which is
//!   a new inode, and a file written over in place changes its size and its time. It costs
//!   one `stat` to write down and one to compare. A build id compiled in would need a build
//!   step that gives every compile its own id, and comparing it with the installed file would
//!   mean running that file; a hash of the file would read all of it for every watcher
//!   started, to say what a new file already says. What a file's identity cannot see is the
//!   same build copied over the old one (`cp`, `touch`): it takes that for a new one, and a
//!   restart is the whole cost.
//! * **The pid** is the watcher's own. Only a record that names the pid in `@agent_watch_pid`
//!   counts, so what a watcher that has gone left behind, or a watcher of a taw that writes no
//!   record over one that did, is never read as the one that runs there now.
//!
//! A watcher started by a taw from before this record has none, and is older than the one that
//! writes it by that fact alone, which is why the first `taw watch status` after an update
//! names every watcher there is.

use std::fmt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The pane option a watcher writes its [`Record`] to.
pub const OPTION: &str = "@agent_watch_build";

/// The command that starts the old watchers again, as `taw watch status` and the docs say it.
pub const RESTART_COMMAND: &str = "taw watch attach-all --restart";

/// A file as the file system tells it from every other: where it lives (device and inode),
/// how big it is, and when it was last written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    device: u64,
    inode: u64,
    size: u64,
    seconds: i64,
    nanoseconds: i64,
}

impl Identity {
    /// The file at `path`, links followed. `None` when nothing that is a file is there.
    pub fn of(path: &Path) -> Option<Identity> {
        let meta = std::fs::metadata(path).ok()?;
        meta.is_file().then(|| Identity {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.size(),
            seconds: meta.mtime(),
            nanoseconds: meta.mtime_nsec(),
        })
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}:{}",
            self.device, self.inode, self.size, self.seconds, self.nanoseconds
        )
    }
}

impl FromStr for Identity {
    type Err = ();

    fn from_str(text: &str) -> Result<Identity, ()> {
        let mut parts = text.split(':');
        let mut next = || parts.next().ok_or(());
        let identity = Identity {
            device: next()?.parse().map_err(|_| ())?,
            inode: next()?.parse().map_err(|_| ())?,
            size: next()?.parse().map_err(|_| ())?,
            seconds: next()?.parse().map_err(|_| ())?,
            nanoseconds: next()?.parse().map_err(|_| ())?,
        };
        // Exactly five numbers: a sixth is not something this wrote.
        match parts.next() {
            None => Ok(identity),
            Some(_) => Err(()),
        }
    }
}

/// What a watcher writes down about itself when it starts: its pid, and the file it was started
/// from as it was then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    pub identity: Identity,
    pub path: PathBuf,
}

impl Record {
    /// The record of the watcher `pid` that was started from `taw_bin`. `None` when there is no
    /// file there to read, which leaves the watcher with nothing to write.
    pub fn of(pid: u32, taw_bin: &Path) -> Option<Record> {
        // A path that is not absolute is read against the directory the watcher is in now: the
        // best that can be said of one, and `current_exe` does not give one.
        let path = std::path::absolute(taw_bin).unwrap_or_else(|_| taw_bin.to_path_buf());
        Some(Record {
            pid,
            identity: Identity::of(&path)?,
            path,
        })
    }

    /// A record as [`fmt::Display`] writes it. `None` for anything else, an empty option
    /// included, and a path that is not absolute: [`Record::of`] writes none, and one would be
    /// read against whatever directory the reader is in.
    pub fn parse(text: &str) -> Option<Record> {
        let (pid, rest) = text.split_once(' ')?;
        let (identity, path) = rest.split_once(' ')?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return None;
        }
        Some(Record {
            pid: pid.parse().ok()?,
            identity: identity.parse().ok()?,
            path,
        })
    }

    /// The record as `set-option` has to be given it: a word that ends in a `;` ends the command
    /// there, unless a backslash comes before the `;`, and tmux takes the backslash back out. A
    /// path ends the record, so one that ends in a `;` would lose it.
    pub fn argument(&self) -> String {
        let text = self.to_string();
        match text.strip_suffix(';') {
            Some(rest) => format!("{rest}\\;"),
            None => text,
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.pid, self.identity, self.path.display())
    }
}

/// How the taw a live watcher runs stands against the file now at the path it was started from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binary {
    /// The file there is the one the watcher was started from.
    Current { path: PathBuf },
    /// Another file is there: the taw was rebuilt or reinstalled since the watcher started.
    Replaced { path: PathBuf },
    /// Nothing is there any more.
    Gone { path: PathBuf },
    /// The watcher wrote no record of its own, which only a taw from before the record does.
    Unrecorded,
}

impl Binary {
    /// Whether the watcher runs a taw other than the one installed where it started.
    pub fn is_old(&self) -> bool {
        !matches!(self, Binary::Current { .. })
    }

    /// The word `taw watch status` puts on the watcher.
    pub fn word(&self) -> &'static str {
        if self.is_old() { "old" } else { "current" }
    }

    /// Why a watcher is old, in plain words. `None` for a current one.
    pub fn note(&self) -> Option<String> {
        match self {
            Binary::Current { .. } => None,
            Binary::Replaced { .. } => {
                Some("the taw it started from has been replaced since it started".to_string())
            }
            Binary::Gone { path } => Some(format!(
                "the taw it started from is no longer at {}",
                path.display()
            )),
            Binary::Unrecorded => Some(
                "started by an older taw, which did not write down which one it was".to_string(),
            ),
        }
    }

    /// The taw to start this watcher from again: the file it started from, which is the one
    /// installed there now, when that is an absolute path to a file that can be run and is the
    /// user's own, and `fallback` (the taw that is doing the restarting) when it is not or there
    /// is no record of where it was. The record is text that anyone who can reach tmux can
    /// write, and what it names is run by the pipe command, so a path that says something else
    /// is not taken at its word.
    pub fn restart_from(&self, fallback: &Path) -> PathBuf {
        self.restart_from_for(fallback, effective_uid())
    }

    /// [`Binary::restart_from`] for the user `owner`, so that a test can ask about a file that
    /// is someone else's.
    fn restart_from_for(&self, fallback: &Path, owner: u32) -> PathBuf {
        match self {
            Binary::Replaced { path } if runnable(path, owner) => path.clone(),
            _ => fallback.to_path_buf(),
        }
    }
}

/// The user this process runs as.
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// Whether `path` is an absolute path to a file that can be run, as the pipe command is told to,
/// and that the user `owner` owns.
fn runnable(path: &Path, owner: u32) -> bool {
    path.is_absolute()
        && std::fs::metadata(path).is_ok_and(|meta| {
            meta.is_file() && meta.permissions().mode() & 0o111 != 0 && meta.uid() == owner
        })
}

/// How the taw of the watcher `watch_pid` stands: its `record` (the option as it was read, or
/// `None` when there is none to read) against what `identity_of` says is at the record's path
/// now. A record that names another pid is no record of this watcher.
pub fn judge(
    record: Option<&Record>,
    watch_pid: u32,
    identity_of: impl Fn(&Path) -> Option<Identity>,
) -> Binary {
    let Some(record) = record.filter(|record| record.pid == watch_pid) else {
        return Binary::Unrecorded;
    };
    let path = record.path.clone();
    match identity_of(&path) {
        Some(now) if now == record.identity => Binary::Current { path },
        Some(_) => Binary::Replaced { path },
        None => Binary::Gone { path },
    }
}

/// What `taw watch status` says to do about the `old` watchers that run an older taw and that a
/// restart would start again: `None` when there are none. While `watching` is off a restart
/// starts nothing, so the sentence says that watching is off, and names no command.
pub fn restart_sentence(old: usize, watching: bool) -> Option<String> {
    let (count, are, it) = match old {
        0 => return None,
        1 => ("1 watcher".to_string(), "is", "it"),
        n => (format!("{n} watchers"), "are", "them"),
    };
    let running = format!("{count} {are} running an older taw than the one installed.");
    Some(if watching {
        format!("{running} Run `{RESTART_COMMAND}` to start {it} again.")
    } else {
        let are_not = if old == 1 {
            "it is not"
        } else {
            "they are not"
        };
        format!(
            "{running} Watching is off (@agent-wizard-watch), so {are_not} started again until watching is turned on."
        )
    })
}

/// What `taw watch attach-all --restart` says when `failed` of the watchers it let go of could not
/// be started again, and their panes have none now: `None` when none failed. A plain
/// `attach-all` attaches a watcher to every pane that has none.
pub fn failed_sentence(failed: usize) -> Option<String> {
    let (count, panes) = match failed {
        0 => return None,
        1 => ("1 watcher".to_string(), "its pane has none"),
        n => (format!("{n} watchers"), "their panes have none"),
    };
    Some(format!(
        "{count} could not be started again, and {panes} now. Run `taw watch attach-all` to try again."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn identity(inode: u64) -> Identity {
        Identity {
            device: 16_777_231,
            inode,
            size: 7_736_288,
            seconds: 1_790_000_000,
            nanoseconds: 123_456_789,
        }
    }

    fn record(pid: u32, inode: u64, path: &str) -> Record {
        Record {
            pid,
            identity: identity(inode),
            path: PathBuf::from(path),
        }
    }

    fn files(entries: &[(&str, u64)]) -> impl Fn(&Path) -> Option<Identity> {
        let map: HashMap<PathBuf, Identity> = entries
            .iter()
            .map(|(path, inode)| (PathBuf::from(path), identity(*inode)))
            .collect();
        move |path| map.get(path).copied()
    }

    #[test]
    fn a_record_is_written_as_the_pid_the_identity_and_the_path() {
        assert_eq!(
            record(4242, 94_855_123, "/opt/taw/bin/taw").to_string(),
            "4242 16777231:94855123:7736288:1790000000:123456789 /opt/taw/bin/taw"
        );
    }

    #[test]
    fn a_record_reads_back_whatever_the_path_holds() {
        for path in [
            "/opt/taw",
            "/Users/some one/with space/taw",
            "/tmp/a:b/taw",
            "/tmp/#{pane_id}/%s/taw",
            "/tmp/taw;",
            "/tmp/caf\u{e9}/taw",
        ] {
            let written = record(7, 99, path);
            assert_eq!(
                Record::parse(&written.to_string()),
                Some(written),
                "path {path:?}"
            );
        }
    }

    #[test]
    fn a_path_that_ends_in_a_semicolon_is_given_to_tmux_with_a_backslash_before_it() {
        let plain = record(7, 99, "/opt/taw");
        assert_eq!(plain.argument(), plain.to_string());
        let odd = record(7, 99, "/opt/taw;");
        assert_eq!(
            odd.argument(),
            "7 16777231:99:7736288:1790000000:123456789 /opt/taw\\;"
        );
        // What tmux stores is the record itself.
        assert_eq!(Record::parse(&odd.to_string()), Some(odd));
    }

    #[test]
    fn what_is_not_a_record_reads_as_none() {
        for text in [
            "",
            "4242",
            "4242 16777231:94855123:7736288:1790000000:123456789",
            "4242 16777231:94855123:7736288:1790000000:123456789 ",
            "x 16777231:94855123:7736288:1790000000:123456789 /taw",
            "4242 1:2:3:4 /taw",
            "4242 1:2:3:4:5:6 /taw",
            "4242 1:2:3:4:x /taw",
            "-1 1:2:3:4:5 /taw",
            "4242 a:b:c:d:e /taw",
            // A path that is not absolute is read against whatever directory the reader is in,
            // and the watcher writes none: what it makes of one is `Record::of`'s absolute path.
            "4242 1:2:3:4:5 taw",
            "4242 1:2:3:4:5 relative/taw",
            "4242 1:2:3:4:5 ./taw",
            "4242 1:2:3:4:5 ../taw",
        ] {
            assert_eq!(Record::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn an_identity_that_is_before_1970_still_reads_back() {
        let early = Identity {
            seconds: -5,
            ..identity(3)
        };
        assert_eq!(early.to_string().parse::<Identity>(), Ok(early));
    }

    #[test]
    fn the_file_at_a_path_is_the_same_until_it_is_replaced_or_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let taw = dir.path().join("taw");
        std::fs::write(&taw, "one").unwrap();
        let first = Identity::of(&taw).unwrap();
        assert_eq!(Identity::of(&taw), Some(first), "nothing happened to it");

        // A new file at the same path, as a build leaves it.
        let next = dir.path().join("taw.next");
        std::fs::write(&next, "one").unwrap();
        std::fs::rename(&next, &taw).unwrap();
        let replaced = Identity::of(&taw).unwrap();
        assert_ne!(
            replaced, first,
            "a new file with the same bytes is a new file"
        );

        // The same file written over, which keeps its inode and moves its size.
        std::fs::write(&taw, "one and more").unwrap();
        let rewritten = Identity::of(&taw).unwrap();
        assert_eq!(rewritten.inode, replaced.inode);
        assert_ne!(rewritten, replaced);

        // The same file with the same size, written a moment later.
        let before = Identity::of(&taw).unwrap();
        std::fs::write(&taw, "ONE AND MORE").unwrap();
        let file = std::fs::File::options().write(true).open(&taw).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let after = Identity::of(&taw).unwrap();
        assert_eq!((after.inode, after.size), (before.inode, before.size));
        assert_ne!(after, before, "only the time moved");
    }

    /// A copy that keeps times (`cp -p`) puts a file of another size back under its old time.
    #[test]
    fn the_size_alone_makes_a_file_another() {
        let dir = tempfile::tempdir().unwrap();
        let taw = dir.path().join("taw");
        std::fs::write(&taw, "one").unwrap();
        let modified = std::fs::metadata(&taw).unwrap().modified().unwrap();
        let before = Identity::of(&taw).unwrap();

        std::fs::write(&taw, "one and more").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&taw)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let after = Identity::of(&taw).unwrap();

        assert_eq!((after.device, after.inode), (before.device, before.inode));
        assert_eq!(
            (after.seconds, after.nanoseconds),
            (before.seconds, before.nanoseconds)
        );
        assert_ne!(after.size, before.size);
        assert_ne!(after, before, "only the size moved");
    }

    /// A test cannot move a file to another device: that takes a second file system, which no
    /// test has any business mounting. Identities made by hand that differ in the device alone
    /// show that it is compared, and that it is written and read back.
    #[test]
    fn the_device_alone_makes_a_file_another() {
        let here = identity(3);
        let elsewhere = Identity {
            device: here.device + 1,
            ..here
        };
        assert_ne!(here, elsewhere);
        assert_ne!(here.to_string(), elsewhere.to_string());
        assert_eq!(elsewhere.to_string().parse::<Identity>(), Ok(elsewhere));

        let written = Record {
            pid: 10,
            identity: here,
            path: PathBuf::from("/opt/taw"),
        };
        assert_eq!(
            judge(Some(&written), 10, |_: &Path| Some(elsewhere)),
            Binary::Replaced {
                path: PathBuf::from("/opt/taw")
            }
        );
    }

    #[test]
    fn a_path_that_is_not_a_file_has_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Identity::of(&dir.path().join("missing")), None);
        assert_eq!(Identity::of(dir.path()), None, "a directory is not a taw");
    }

    #[test]
    fn a_link_has_the_identity_of_what_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        let link = dir.path().join("taw");
        std::fs::write(&build, "one").unwrap();
        std::os::unix::fs::symlink(&build, &link).unwrap();
        assert_eq!(Identity::of(&link), Identity::of(&build));
        let other = dir.path().join("other");
        std::fs::write(&other, "one").unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_ne!(Identity::of(&link), Identity::of(&build), "the link moved");
    }

    #[test]
    fn a_record_is_made_of_the_file_at_the_path_and_of_nothing_when_there_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let taw = dir.path().join("taw");
        assert_eq!(Record::of(5, &taw), None);
        std::fs::write(&taw, "one").unwrap();
        let made = Record::of(5, &taw).unwrap();
        assert_eq!(made.pid, 5);
        assert_eq!(made.path, taw);
        assert_eq!(Some(made.identity), Identity::of(&taw));
    }

    #[test]
    fn a_relative_path_is_recorded_as_an_absolute_one() {
        let made = Record::of(5, Path::new("Cargo.toml")).unwrap();
        assert!(made.path.is_absolute(), "{}", made.path.display());
        assert!(made.path.ends_with("Cargo.toml"));
    }

    #[test]
    fn the_file_there_is_current_another_is_replaced_and_none_is_gone() {
        let written = record(10, 1, "/opt/taw");
        assert_eq!(
            judge(Some(&written), 10, files(&[("/opt/taw", 1)])),
            Binary::Current {
                path: PathBuf::from("/opt/taw")
            }
        );
        assert_eq!(
            judge(Some(&written), 10, files(&[("/opt/taw", 2)])),
            Binary::Replaced {
                path: PathBuf::from("/opt/taw")
            }
        );
        assert_eq!(
            judge(Some(&written), 10, files(&[])),
            Binary::Gone {
                path: PathBuf::from("/opt/taw")
            }
        );
        // Only the file at the watcher's own path is asked about.
        assert_eq!(
            judge(Some(&written), 10, files(&[("/elsewhere/taw", 1)])),
            Binary::Gone {
                path: PathBuf::from("/opt/taw")
            }
        );
    }

    #[test]
    fn no_record_and_a_record_of_another_pid_are_both_unrecorded() {
        let written = record(10, 1, "/opt/taw");
        let there = files(&[("/opt/taw", 1)]);
        assert_eq!(judge(None, 10, &there), Binary::Unrecorded);
        assert_eq!(judge(Some(&written), 11, &there), Binary::Unrecorded);
        assert_eq!(judge(Some(&written), 0, &there), Binary::Unrecorded);
    }

    #[test]
    fn only_a_current_watcher_is_not_old_and_each_old_one_says_why() {
        let path = PathBuf::from("/opt/taw");
        let current = Binary::Current { path: path.clone() };
        assert!(!current.is_old());
        assert_eq!(current.word(), "current");
        assert_eq!(current.note(), None);

        for (binary, note) in [
            (
                Binary::Replaced { path: path.clone() },
                "the taw it started from has been replaced since it started",
            ),
            (
                Binary::Gone { path: path.clone() },
                "the taw it started from is no longer at /opt/taw",
            ),
            (
                Binary::Unrecorded,
                "started by an older taw, which did not write down which one it was",
            ),
        ] {
            assert!(binary.is_old(), "{binary:?}");
            assert_eq!(binary.word(), "old", "{binary:?}");
            assert_eq!(binary.note().as_deref(), Some(note), "{binary:?}");
        }
    }

    #[test]
    fn a_watcher_starts_again_from_where_it_started_when_there_is_something_to_run_there() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("taw");
        let fallback = dir.path().join("fallback");
        std::fs::write(&installed, "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable(&installed);

        let replaced = Binary::Replaced {
            path: installed.clone(),
        };
        assert_eq!(replaced.restart_from(&fallback), installed);

        // Not something that can be run: the taw that is restarting is.
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(replaced.restart_from(&fallback), fallback);

        // Nothing there, or nothing known of where it was.
        std::fs::remove_file(&installed).unwrap();
        assert_eq!(replaced.restart_from(&fallback), fallback);
        assert_eq!(
            Binary::Gone { path: installed }.restart_from(&fallback),
            fallback
        );
        assert_eq!(Binary::Unrecorded.restart_from(&fallback), fallback);
    }

    /// A recorded path is text anyone who can reach tmux can write, and a path that is not
    /// absolute names whatever is at that place in the directory the pipe command runs in.
    #[test]
    fn a_recorded_path_that_is_not_absolute_is_never_run() {
        // A file that can be run, found from the directory the unit tests run in.
        let relative = Path::new("scripts/install.sh");
        let meta = std::fs::metadata(relative).expect("the repository's own installer");
        assert!(meta.is_file() && meta.permissions().mode() & 0o111 != 0);

        let fallback = Path::new("/opt/typed/taw");
        let replaced = Binary::Replaced {
            path: relative.to_path_buf(),
        };
        assert_eq!(replaced.restart_from(fallback), fallback);
    }

    /// A file somebody else owns is not one to start from, whoever is at that path: a directory
    /// anyone can write to (`/tmp`) is where one would be put.
    #[test]
    fn a_recorded_file_that_is_another_users_is_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("taw");
        let fallback = dir.path().join("fallback");
        std::fs::write(&installed, "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable(&installed);
        let replaced = Binary::Replaced {
            path: installed.clone(),
        };

        let me = effective_uid();
        assert_eq!(replaced.restart_from_for(&fallback, me), installed);
        assert_eq!(replaced.restart_from_for(&fallback, me + 1), fallback);
        assert_eq!(
            replaced.restart_from(&fallback),
            installed,
            "this user's own"
        );
    }

    #[test]
    fn the_restart_sentence_agrees_with_the_count() {
        assert_eq!(restart_sentence(0, true), None);
        assert_eq!(
            restart_sentence(1, true).as_deref(),
            Some(
                "1 watcher is running an older taw than the one installed. Run `taw watch attach-all --restart` to start it again."
            )
        );
        assert_eq!(
            restart_sentence(3, true).as_deref(),
            Some(
                "3 watchers are running an older taw than the one installed. Run `taw watch attach-all --restart` to start them again."
            )
        );
    }

    #[test]
    fn while_watching_is_off_the_sentence_says_so_and_names_no_command_to_run() {
        assert_eq!(restart_sentence(0, false), None);
        assert_eq!(
            restart_sentence(1, false).as_deref(),
            Some(
                "1 watcher is running an older taw than the one installed. Watching is off (@agent-wizard-watch), so it is not started again until watching is turned on."
            )
        );
        assert_eq!(
            restart_sentence(3, false).as_deref(),
            Some(
                "3 watchers are running an older taw than the one installed. Watching is off (@agent-wizard-watch), so they are not started again until watching is turned on."
            )
        );
        for off in [restart_sentence(1, false), restart_sentence(3, false)] {
            assert!(!off.unwrap().contains(RESTART_COMMAND));
        }
    }

    #[test]
    fn the_failure_sentence_says_which_panes_have_no_watcher_now_and_what_to_run() {
        assert_eq!(failed_sentence(0), None);
        assert_eq!(
            failed_sentence(1).as_deref(),
            Some(
                "1 watcher could not be started again, and its pane has none now. Run `taw watch attach-all` to try again."
            )
        );
        assert_eq!(
            failed_sentence(2).as_deref(),
            Some(
                "2 watchers could not be started again, and their panes have none now. Run `taw watch attach-all` to try again."
            )
        );
    }

    #[test]
    fn the_status_listing_reads_the_option_this_module_names() {
        // `watch::status` spells the option as a literal inside a format, which a constant
        // cannot be put into.
        assert_eq!(OPTION, "@agent_watch_build");
        assert!(super::super::STATUS_FORMAT.contains(OPTION));
    }
}
