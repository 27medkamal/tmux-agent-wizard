//! The hooks log: `<state>/logs/hooks.log`, one line for each hook that failed.
//!
//! A hook runs in the background, where nobody is looking, so the moment it fails is the
//! moment nothing says so. A hook that exits non-zero, one that is stopped for running past
//! `[hooks] timeout_seconds`, and one that could not be started each leave a line here, with
//! the last thing the hook wrote to stderr. A hook that worked leaves nothing. `taw hooks log`
//! prints the end of the file.
//!
//! Every entry is one line, so a tail is a set of whole entries, and one `write` call, so
//! hooks that fail at the same moment do not interleave.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths::Paths;

/// The file's name inside the logs directory.
pub const FILE: &str = "hooks.log";

/// The size past which the file is moved to `hooks.log.1` before the next entry is written:
/// one older generation, which is plenty for a record of failures.
const MAX_BYTES: u64 = 256 * 1024;

/// The longest excerpt of a hook's stderr an entry carries.
const MAX_EXCERPT_CHARS: usize = 300;

/// Where the hooks log lives.
pub fn path(paths: &Paths) -> PathBuf {
    paths.logs_dir.join(FILE)
}

/// Appends one entry: `<time> <event> <outcome>: <command>`, and ` | <stderr>` when the hook
/// wrote something. Best effort by design: a log that cannot be written must never turn a
/// hook's failure into a second one, and there is nobody to tell.
pub fn record(log: &Path, event: &str, outcome: &str, command: &str, stderr: &str) {
    let mut line = format!(
        "{} {event} {outcome}: {}",
        utc(unix_seconds()),
        one_line(command, usize::MAX)
    );
    if let Some(last) = last_line(stderr) {
        line.push_str(" | ");
        line.push_str(&one_line(last, MAX_EXCERPT_CHARS));
    }
    line.push('\n');
    if append(log, &line).is_err() {
        log::debug!("could not write to {}", log.display());
    }
}

fn append(log: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(log).is_ok_and(|meta| meta.len() > MAX_BYTES) {
        let _ = std::fs::rename(log, log.with_extension("log.1"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?
        .write_all(line.as_bytes())
}

/// The last `lines` lines of the log, oldest first; none when there is no log yet.
pub fn tail(log: &Path, lines: usize) -> Vec<String> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|line| line.to_string())
        .collect()
}

/// The last line of `stderr` with something on it: the hook's verdict, as it is for the
/// popup's toast.
pub fn last_line(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// `text` on one line, and at most `max` characters, `...` in place of what was cut. A
/// command or a hook's stderr can hold newlines, and an entry must stay one line.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let mut cut: String = flat.chars().take(max).collect();
    cut.push_str("...");
    cut
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `secs` after the epoch as `2024-05-06 10:11:12 UTC`. The date is the proleptic Gregorian
/// one, worked out from the day number (Howard Hinnant's `civil_from_days`) so that a log
/// line needs no time crate.
pub fn utc(secs: u64) -> String {
    let (days, of_day) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_written_in_utc_down_to_the_second() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_000_000_000), "2001-09-09 01:46:40 UTC");
        assert_eq!(utc(1_790_000_000), "2026-09-21 14:13:20 UTC");
    }

    #[test]
    fn leap_days_are_counted_and_a_century_is_not_one() {
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(utc(4_102_444_800), "2100-01-01 00:00:00 UTC");
    }

    #[test]
    fn an_entry_is_one_line_naming_the_event_the_outcome_the_command_and_the_last_thing_said() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("logs").join(FILE);

        record(
            &log,
            "agent.done",
            "failed (exit status 6)",
            "~/bin/push --topic mine",
            "connecting\ncurl: (6) Could not resolve host: ntfy.sh\n\n",
        );

        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1);
        let entry = text.lines().next().unwrap();
        // `2024-05-06 10:11:12 UTC ` is 24 characters.
        assert!(entry[..4].chars().all(|c| c.is_ascii_digit()), "{entry}");
        assert_eq!(
            &entry[24..],
            "agent.done failed (exit status 6): ~/bin/push --topic mine | curl: (6) Could not resolve host: ntfy.sh"
        );
    }

    #[test]
    fn a_hook_that_said_nothing_leaves_an_entry_with_no_excerpt() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(FILE);

        record(&log, "notification", "timed out after 30 s", "sleep 60", "");

        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.trim_end()
                .ends_with("notification timed out after 30 s: sleep 60"),
            "{text}"
        );
    }

    #[test]
    fn a_command_and_a_message_with_newlines_stay_on_one_line() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(FILE);

        record(
            &log,
            "agent.idle",
            "failed (exit status 1)",
            "echo one\necho two\t\x1b[31m",
            "x\ny\x07",
        );

        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text:?}");
        assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    }

    #[test]
    fn a_long_excerpt_is_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(FILE);

        record(
            &log,
            "agent.idle",
            "failed (exit status 1)",
            "x",
            &"e".repeat(1000),
        );

        let text = std::fs::read_to_string(&log).unwrap();
        let excerpt = text.trim_end().rsplit(" | ").next().unwrap();
        assert_eq!(excerpt.chars().count(), MAX_EXCERPT_CHARS + 3);
        assert!(excerpt.ends_with("..."));
    }

    #[test]
    fn the_tail_is_the_last_entries_oldest_first_and_nothing_without_a_log() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(FILE);
        assert!(tail(&log, 5).is_empty());

        for n in 1..=5 {
            record(
                &log,
                "agent.done",
                &format!("failed (exit status {n})"),
                "true",
                "",
            );
        }

        let last_two = tail(&log, 2);
        assert_eq!(last_two.len(), 2);
        assert!(last_two[0].contains("exit status 4"), "{last_two:?}");
        assert!(last_two[1].contains("exit status 5"), "{last_two:?}");
        assert_eq!(tail(&log, 100).len(), 5);
    }

    #[test]
    fn an_oversized_log_is_moved_aside_once_and_starts_again() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join(FILE);
        std::fs::write(&log, "x".repeat(MAX_BYTES as usize + 1)).unwrap();

        record(&log, "agent.done", "failed (exit status 1)", "true", "");

        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text:?}");
        let old = std::fs::read_to_string(tmp.path().join("hooks.log.1")).unwrap();
        assert_eq!(old.len(), MAX_BYTES as usize + 1);
    }

    #[test]
    fn the_last_line_is_the_last_one_with_anything_on_it() {
        assert_eq!(last_line("a\nb  \n\n \n"), Some("b"));
        assert_eq!(last_line(""), None);
        assert_eq!(last_line(" \n\t"), None);
    }
}
