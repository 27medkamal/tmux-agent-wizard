//! The watcher's log file.
//!
//! A watcher is one process per pane, started by tmux with no terminal of its own, so
//! logging is off unless `$TAW_LOG` asks for it: `TAW_LOG=debug` gives
//! `<state>/logs/watch-<pane>.log`.
//!
//! The level is read from this process's environment, inherited from the tmux server
//! that spawned the `pipe-pane` child and fixed at spawn. `tmux set-environment -g
//! TAW_LOG …` on an already-running server therefore does not reach a watcher that is
//! already attached — only ones started afterwards (docs/ARCHITECTURE.md, watcher
//! logging).
//!
//! This module is `watch::log`, so `watch::mod` imports the `log` crate's macros by name
//! (`use ::log::{debug, info, warn}`) rather than pathing through a shadowed `log::`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use log::LevelFilter;

/// Environment variable holding the watcher's log level (`off`, `error`, `warn`, `info`,
/// `debug`, `trace`). Unset or empty means no logger at all.
const LEVEL_ENV: &str = "TAW_LOG";

/// Level used when `$TAW_LOG` is set to something unparseable.
const FALLBACK_LEVEL: LevelFilter = LevelFilter::Error;

/// Size at which the log file is rotated to `<name>.log.1` (one generation, no gzip:
/// this is a debugging aid, not an audit trail).
const MAX_BYTES: u64 = 1024 * 1024;

/// Installs the per-pane `WriteLogger`, or does nothing when `$TAW_LOG` is unset/empty
/// or `off`.
///
/// The log is rotated on **write**, by [`RotatingFile`], not once at startup. A watcher is
/// one process per pane that lives as long as the pane does — days, for a pane someone
/// leaves an agent in — so a check that only runs at startup is a check that never runs
/// again: at `TAW_LOG=debug` the file grew without bound past its 1 MB cap for the
/// whole life of the process.
///
/// Called once per process, by the `taw watch run` CLI entry point. A second call fails
/// (the `log` crate allows one global logger), which is why `run`/`run_with_reader` never
/// call it themselves — tests drive the loop in-process.
pub fn init_logging(paths: &crate::paths::Paths, pane: &str) -> anyhow::Result<()> {
    let Some(spec) = std::env::var(LEVEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let level = spec.parse::<LevelFilter>().unwrap_or(FALLBACK_LEVEL);
    if level == LevelFilter::Off {
        return Ok(());
    }

    std::fs::create_dir_all(&paths.logs_dir)
        .with_context(|| format!("creating {}", paths.logs_dir.display()))?;
    let path = log_path(&paths.logs_dir, pane);
    let file = RotatingFile::open(path.clone(), MAX_BYTES)?;
    simplelog::WriteLogger::init(level, simplelog::Config::default(), file)
        .context("installing the watcher's log writer")?;
    log::debug!("logging pane {pane} at {level} to {}", path.display());
    Ok(())
}

/// `<logs_dir>/watch-<pane>.log`, with anything that is not a safe file-name character
/// folded to `-` (a pane can be named by a tmux *target* such as `sess:1.0`, which must
/// not be able to escape the log directory).
fn log_path(logs_dir: &Path, pane: &str) -> PathBuf {
    let safe: String = pane
        .chars()
        .map(|c| match c {
            '%' | '-' | '_' | '@' | '$' => c,
            c if c.is_ascii_alphanumeric() => c,
            _ => '-',
        })
        .collect();
    logs_dir.join(format!("watch-{safe}.log"))
}

/// An append-only log file that rotates itself to `<name>.log.1` before any write that
/// would take it past `max` bytes. One generation, no gzip: this is a debugging aid, not
/// an audit trail.
///
/// The size is tracked in memory rather than re-`stat`ed per write — the file is only ever
/// appended to by this process, so the count it starts from (the file's length at open)
/// stays right, and a log line must not cost a syscall to decide it fits.
struct RotatingFile {
    path: PathBuf,
    max: u64,
    file: File,
    written: u64,
}

impl RotatingFile {
    fn open(path: PathBuf, max: u64) -> anyhow::Result<RotatingFile> {
        let file = append_to(&path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(RotatingFile {
            path,
            max,
            file,
            written,
        })
    }

    /// Renames the current file aside and opens a fresh one. Best effort: a rotation that
    /// fails leaves the oversized file in place and keeps logging to it, because losing a
    /// watcher over its own debug log would be much worse than an oversized debug log.
    fn rotate(&mut self) {
        let rotated = self.path.with_extension("log.1");
        if std::fs::rename(&self.path, &rotated).is_err() {
            return;
        }
        match append_to(&self.path) {
            Ok(file) => {
                self.file = file;
                self.written = 0;
            }
            // The old file is already renamed away and the handle we hold still points at
            // it, so writes keep landing there under its new name. Nothing is lost.
            Err(_) => self.written = 0,
        }
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Rotated *before* the write that would cross the cap, so the file never exceeds
        // it. A single record larger than the whole cap still gets written, to its own
        // freshly rotated file — truncating a log line would be worse than overshooting.
        if self.written > 0 && self.written.saturating_add(buf.len() as u64) > self.max {
            self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn append_to(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_keeps_pane_ids_readable_and_folds_path_separators_away() {
        let dir = Path::new("/state/logs");
        assert_eq!(log_path(dir, "%12"), Path::new("/state/logs/watch-%12.log"));
        assert_eq!(
            log_path(dir, "sess:1.0"),
            Path::new("/state/logs/watch-sess-1-0.log")
        );
        assert_eq!(
            log_path(dir, "../../etc/passwd"),
            Path::new("/state/logs/watch-------etc-passwd.log")
        );
    }

    #[test]
    fn a_long_lived_writer_rotates_itself_instead_of_growing_without_bound() {
        // The finding: a watcher lives for days, so rotating only at startup means never
        // rotating. Writing well past the cap must leave the live file under it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("watch-%1.log");
        let rotated = tmp.path().join("watch-%1.log.1");

        let mut log = RotatingFile::open(path.clone(), 100).unwrap();
        for i in 0..50 {
            writeln!(log, "line {i:->20}").unwrap();
        }
        log.flush().unwrap();

        let live = std::fs::metadata(&path).unwrap().len();
        assert!(
            live <= 100,
            "the live log must stay under the cap, got {live} bytes"
        );
        assert!(rotated.exists(), "one generation must be kept");
        assert!(std::fs::metadata(&rotated).unwrap().len() <= 100);
    }

    #[test]
    fn a_writer_under_the_cap_never_rotates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("watch-%2.log");
        let rotated = tmp.path().join("watch-%2.log.1");

        let mut log = RotatingFile::open(path.clone(), 1024).unwrap();
        write!(log, "small").unwrap();
        log.flush().unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"small");
        assert!(!rotated.exists(), "nothing to rotate under the cap");
    }

    #[test]
    fn a_writer_continues_an_existing_log_and_counts_what_is_already_there() {
        // Reopened by a restarted watcher: the bytes already on disk count towards the
        // cap, or a pane restarted often enough would never rotate at all.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("watch-%3.log");
        let rotated = tmp.path().join("watch-%3.log.1");
        std::fs::write(&path, vec![b'x'; 95]).unwrap();

        let mut log = RotatingFile::open(path.clone(), 100).unwrap();
        writeln!(log, "the straw").unwrap(); // 10 bytes: 95 + 10 is over the cap
        log.flush().unwrap();

        assert!(rotated.exists(), "the pre-existing bytes must count");
        assert_eq!(std::fs::metadata(&rotated).unwrap().len(), 95);
        assert_eq!(std::fs::read(&path).unwrap(), b"the straw\n");
    }

    #[test]
    fn a_single_record_larger_than_the_cap_is_written_whole() {
        // Overshooting beats truncating a log line into something unparseable.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("watch-%4.log");

        let mut log = RotatingFile::open(path.clone(), 8).unwrap();
        write!(log, "first").unwrap();
        let huge = "y".repeat(64);
        write!(log, "{huge}").unwrap();
        log.flush().unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), huge);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("watch-%4.log.1")).unwrap(),
            "first"
        );
    }
}
