//! What taw does when a tmux server starts (`taw server started`, which the plugin runs each time it
//! loads): it writes down which server this is, and, when the server before it on the same socket is
//! gone and crashed, keeps the crash report macOS wrote for it and says where it is.
//!
//! macOS writes the report of a crashed program into `~/Library/Logs/DiagnosticReports`, where it can
//! be removed later. The copy is taw's own, in `<state>/tmux-crashes` (a private folder of private
//! files), and is never overwritten.
//!
//! The record is `<state>/servers/<name made from the socket's path>.json`: the pid and start time of
//! the last server that started on that socket. A server that is not the one in the record is a new
//! one; the same pid with the same start time is the plugin loading again (a `source-file` of
//! tmux.conf, say) and does nothing.
//!
//! A report is a `tmux-*.ips` file: a JSON header on its first line, then the JSON body, whose `pid`,
//! `procName` and `captureTime` are what is read here. A killed server (`kill-server`, the last
//! session closing) leaves no report, which is why not finding one is no error and says nothing.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::notify::{Class, Notification, NotifiedAgent, SoundChoice};

/// Where the records of servers go, under the state directory.
pub const SERVERS_DIR: &str = "servers";
/// Where the kept crash reports go, under the state directory.
pub const CRASHES_DIR: &str = "tmux-crashes";

/// How long to look for the report of a server that is gone, and how often.
pub const WATCH_FOR: Duration = Duration::from_secs(120);
pub const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long to wait for a client to be attached, for a toast that needs one.
pub const CLIENT_WAIT: Duration = Duration::from_secs(600);

/// A report larger than this is not read: a crash report is a few hundred kilobytes.
const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;

/// The server a record is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    pub start_time: u64,
}

/// What the plugin loading says about the server it loaded on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub socket_path: String,
    pub pid: u32,
    pub start_time: u64,
}

/// What a server's start turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// No record before, or one that could not be read: the first server taw has seen on this socket.
    First,
    /// The server the record names, loaded again.
    Reload,
    /// Another server than the one the record names, which is `last`.
    Replaced { last: Record },
}

/// The file name of the record of the server on `socket_path`. Every byte that is not a letter, a
/// digit, `-` or `_` is written as `%` and its two hex digits (a `/` is `%2F`), so two paths never
/// share a name, a name never holds a separator, and a `.` can never make `..`. A path whose name
/// would be long keeps its end (the socket's own name) and a hash of the whole.
pub fn record_file_name(socket_path: &str) -> String {
    let mut name = String::new();
    for byte in socket_path.trim_start_matches('/').bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => name.push(char::from(byte)),
            _ => {
                let _ = write!(name, "%{byte:02X}");
            }
        }
    }
    if name.is_empty() {
        name.push_str("server");
    }
    if name.len() > 200 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in socket_path.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        name = format!("{}-{hash:016x}", &name[name.len() - 150..]);
    }
    format!("{name}.json")
}

/// Reads a record; `None` for a file that is not there or does not hold one.
pub fn read_record(path: &Path) -> Option<Record> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Writes a record whole: to a file beside it and then over it, so a reader never sees half of one.
pub fn write_record(path: &Path, record: &Record) -> io::Result<()> {
    let mut text = serde_json::to_string(record).map_err(io::Error::other)?;
    text.push('\n');
    crate::atomic_file::write_whole(path, &text, "record")
}

/// Records `server` in `servers_dir` and says what its start was. The same pid and start time as
/// the record's change nothing (a server's start time never changes, so a new server that was given
/// the old pid, after a reboot say, has another one); any other server replaces the record.
pub fn note_server(servers_dir: &Path, server: &Server) -> io::Result<Seen> {
    let path = servers_dir.join(record_file_name(&server.socket_path));
    let last = read_record(&path);
    if last
        .as_ref()
        .is_some_and(|last| last.pid == server.pid && last.start_time == server.start_time)
    {
        return Ok(Seen::Reload);
    }
    write_record(
        &path,
        &Record {
            pid: server.pid,
            start_time: server.start_time,
        },
    )?;
    Ok(match last {
        Some(last) => Seen::Replaced { last },
        None => Seen::First,
    })
}

/// A crash report of tmux that has been read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub path: PathBuf,
    pub pid: u32,
    /// `captureTime`, as unix seconds.
    pub captured_unix: i64,
}

/// The report in `path`, when it is a crash report of a program called tmux that says its pid and
/// when it was captured; `None` for anything else, a file that does not parse included.
pub fn read_report(path: &Path) -> Option<Report> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_REPORT_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut values = serde_json::Deserializer::from_str(&text).into_iter::<Value>();
    // The first value is the report's own header, the second its body.
    values.next()?.ok()?;
    let body = values.next()?.ok()?;
    if body.get("procName")?.as_str()? != "tmux" {
        return None;
    }
    Some(Report {
        path: path.to_path_buf(),
        pid: u32::try_from(body.get("pid")?.as_u64()?).ok()?,
        captured_unix: parse_capture_time(body.get("captureTime")?.as_str()?)?,
    })
}

/// The newest report in `dir` that is of the tmux with `pid` and was captured at `not_before` (unix
/// seconds) or later. The time keeps a report of an older server, which had the same pid once the
/// system gave it out again, from being taken for this one's.
pub fn find_report(dir: &Path, pid: u32, not_before: i64) -> Option<Report> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("tmux-") && n.ends_with(".ips"))
        })
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|path| read_report(path))
        .filter(|report| report.pid == pid && report.captured_unix >= not_before)
        .max_by_key(|report| report.captured_unix)
}

/// How many times to look, `poll` apart, to cover `watch`: the first look is at once.
pub fn looks(watch: Duration, poll: Duration) -> u32 {
    let poll = poll.as_millis().max(1);
    u32::try_from(watch.as_millis().div_ceil(poll))
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

/// Makes `dest` a link to `tmp` and says whether this call made it: `false` when a file of that name
/// is there already, which is left as it is. A link fails for a name that is taken (a copy or a
/// rename would not), so of two runs at once exactly one is told `true`, and only that one keeps
/// the report and says so.
pub fn link_new(tmp: &Path, dest: &Path) -> io::Result<bool> {
    match fs::hard_link(tmp, dest) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

/// Copies `report` into `dir` under its own name, and returns where, or `None` when a file of that
/// name is there already, which is left as it is. The folder is made private (0700) and the copy is
/// private (0600) whatever the report's own mode is. The copy is made beside it and linked into
/// place with `link_new`.
pub fn keep(report: &Report, dir: &Path) -> io::Result<Option<PathBuf>> {
    let name = report
        .path
        .file_name()
        .ok_or_else(|| io::Error::other("a report with no file name"))?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let dest = dir.join(name);
    if dest.exists() {
        return Ok(None);
    }
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&tmp);
    let copied = copy_private(&report.path, &tmp);
    let linked = copied.and_then(|()| link_new(&tmp, &dest));
    let _ = fs::remove_file(&tmp);
    Ok(linked?.then_some(dest))
}

/// Copies `from` to the new file `to`, which is made 0600 from the start, whatever `from`'s mode is.
fn copy_private(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = fs::File::open(from)?;
    let mut copy = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    io::copy(&mut source, &mut copy)?;
    // The mode given to `open` is cut by the umask, never widened; this says it outright.
    copy.set_permissions(fs::Permissions::from_mode(0o600))
}

/// `path` with the home directory written as `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty())
        && let Ok(rest) = path.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// `HH:MM:SS` of a unix time at `offset` seconds from UTC.
pub fn clock(unix: i64, offset: i64) -> String {
    let seconds = unix.saturating_add(offset).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

/// The notification that tells where the report is kept: about no pane and no agent, and with no
/// sound, as `taw notification show` builds one.
pub fn toast(kept: &Path, report: &Report, home: Option<&Path>, offset: i64) -> Notification {
    Notification {
        class: Class::Done,
        title: format!("tmux crashed at {}", clock(report.captured_unix, offset)),
        body: format!(
            "Its crash report is kept at {}. taw session restore brings back what it had.",
            tilde(kept, home)
        ),
        pane_id: None,
        sound: SoundChoice::None,
        agent: NotifiedAgent::default(),
    }
}

/// A report's `captureTime` (`2026-10-16 00:12:33.4518 +0200`) as unix seconds. The fraction of a
/// second is dropped; a missing zone is UTC.
pub fn parse_capture_time(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once([' ', 'T'])?;
    let rest = rest.trim();
    let (time, zone) = match rest.find(['+', '-', 'Z', ' ']) {
        Some(at) => (&rest[..at], rest[at..].trim()),
        None => (rest, ""),
    };
    let mut ymd = date.split('-');
    let year: i64 = ymd.next()?.parse().ok()?;
    let month: i64 = ymd.next()?.parse().ok()?;
    let day: i64 = ymd.next()?.parse().ok()?;
    if ymd.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut hms = time.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.split('.').next()?.parse().ok()?;
    if hms.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset = match zone {
        "" | "Z" => 0,
        zone => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
            if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            sign * (digits[..2].parse::<i64>().ok()? * 3_600
                + digits[2..].parse::<i64>().ok()? * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Days from 1970-01-01 to a calendar day (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// How the places, the clock and the screen are reached: the real ones for the verb, a recording
/// stand-in for a test.
pub trait Effects {
    fn sleep(&mut self, how_long: Duration);
    /// Whether a client is attached to the server.
    fn client_attached(&mut self) -> bool;
    fn announce(&mut self, notification: &Notification);
    /// The local time zone's distance from UTC at a unix time, in seconds.
    fn utc_offset(&self, unix: i64) -> i64;
}

/// Where the reports are looked for and the copy is kept, and how long to look and wait.
pub struct Search<'a> {
    pub reports_dir: &'a Path,
    pub crashes_dir: &'a Path,
    pub home: Option<&'a Path>,
    pub watch: Duration,
    pub poll: Duration,
    pub client_wait: Duration,
    /// Whether the toast needs a client attached to be seen (`herdr` and `terminal` delivery).
    pub needs_client: bool,
}

/// What came of the search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The copy this run made.
    pub kept: Option<PathBuf>,
    /// Whether it told the user.
    pub announced: bool,
}

/// Looks for the report of the server `last`, which is gone, for `search.watch`, and keeps the copy
/// and tells the user when it finds one. Only a run that makes the copy announces: another that
/// finds the file there already, as a second load at the same moment would, says nothing.
pub fn keep_report_of(last: &Record, search: &Search, effects: &mut impl Effects) -> Found {
    let not_before = i64::try_from(last.start_time).unwrap_or(i64::MAX);
    let mut report = None;
    for look in 0..looks(search.watch, search.poll) {
        if look > 0 {
            effects.sleep(search.poll);
        }
        report = find_report(search.reports_dir, last.pid, not_before);
        if report.is_some() {
            break;
        }
    }
    let Some(report) = report else {
        return Found::default();
    };
    let kept = match keep(&report, search.crashes_dir) {
        Ok(Some(kept)) => kept,
        Ok(None) => return Found::default(),
        Err(err) => {
            log::warn!(
                "could not keep the crash report {}: {err}",
                report.path.display()
            );
            return Found::default();
        }
    };
    if search.needs_client && !wait_for_client(search, effects) {
        return Found {
            kept: Some(kept),
            announced: false,
        };
    }
    let offset = effects.utc_offset(report.captured_unix);
    effects.announce(&toast(&kept, &report, search.home, offset));
    Found {
        kept: Some(kept),
        announced: true,
    }
}

/// Waits for a client to be attached, looking every `search.poll` for at most `search.client_wait`.
fn wait_for_client(search: &Search, effects: &mut impl Effects) -> bool {
    for look in 0..looks(search.client_wait, search.poll) {
        if look > 0 {
            effects.sleep(search.poll);
        }
        if effects.client_attached() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn server(pid: u32) -> Server {
        Server {
            socket_path: "/private/tmp/tmux-501/default".to_string(),
            pid,
            start_time: 1_000,
        }
    }

    #[test]
    fn the_record_file_name_is_made_from_the_socket_path() {
        assert_eq!(
            record_file_name("/private/tmp/tmux-501/default"),
            "private%2Ftmp%2Ftmux-501%2Fdefault.json"
        );
        // A space and a dot are written out, so no name holds a separator or is "..".
        assert_eq!(
            record_file_name("/tmp/my dir/a.sock"),
            "tmp%2Fmy%20dir%2Fa%2Esock.json"
        );
        assert_eq!(record_file_name("/.."), "%2E%2E.json");
        // A `-L` name lives in tmux's own folder, and is the end of the path.
        assert_eq!(
            record_file_name("/private/tmp/tmux-501/work_1"),
            "private%2Ftmp%2Ftmux-501%2Fwork_1.json"
        );
        // Two paths never share a name.
        assert_ne!(record_file_name("/a b/c"), record_file_name("/a-b/c"));
        assert_ne!(record_file_name("/a/b"), record_file_name("/a%2Fb"));
        assert_eq!(record_file_name(""), "server.json");
    }

    #[test]
    fn a_long_socket_path_still_gives_a_name_a_file_can_have() {
        let long = format!("/{}/sock", "d".repeat(400));
        let other = format!("/{}/sock", "e".repeat(400));
        let name = record_file_name(&long);
        assert!(name.len() < 255, "{name}");
        assert!(name.ends_with(".json"));
        assert!(name.contains("sock"), "{name}");
        assert_ne!(name, record_file_name(&other));
    }

    #[test]
    fn a_record_is_written_whole_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers").join("a.json");
        assert_eq!(read_record(&path), None);
        let record = Record {
            pid: 4242,
            start_time: 1_700_000_000,
        };
        write_record(&path, &record).unwrap();
        assert_eq!(read_record(&path), Some(record.clone()));
        let again = Record {
            pid: 7,
            start_time: 9,
        };
        write_record(&path, &again).unwrap();
        assert_eq!(read_record(&path), Some(again));
        // Nothing is left beside it.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // A file that holds no record is no record.
        fs::write(&path, "not json").unwrap();
        assert_eq!(read_record(&path), None);
    }

    #[test]
    fn a_new_server_is_recorded_and_the_same_one_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(record_file_name(&server(10).socket_path));
        assert_eq!(note_server(dir.path(), &server(10)).unwrap(), Seen::First);
        assert_eq!(read_record(&path).unwrap().pid, 10);

        // The same pid and start time is a reload: the record is not touched.
        assert_eq!(note_server(dir.path(), &server(10)).unwrap(), Seen::Reload);
        assert_eq!(read_record(&path).unwrap().start_time, 1_000);

        // The same pid with another start time is a new server that was given the old pid (after a
        // reboot): it replaces the record and says whose it was.
        let mut reused = server(10);
        reused.start_time = 5_000;
        assert_eq!(
            note_server(dir.path(), &reused).unwrap(),
            Seen::Replaced {
                last: Record {
                    pid: 10,
                    start_time: 1_000
                }
            }
        );
        assert_eq!(read_record(&path).unwrap().start_time, 5_000);
        write_record(
            &path,
            &Record {
                pid: 10,
                start_time: 1_000,
            },
        )
        .unwrap();

        // Another pid replaces the record and says whose it was.
        assert_eq!(
            note_server(dir.path(), &server(11)).unwrap(),
            Seen::Replaced {
                last: Record {
                    pid: 10,
                    start_time: 1_000
                }
            }
        );
        assert_eq!(read_record(&path).unwrap().pid, 11);
        // A record that cannot be read is no last server.
        fs::write(&path, "{").unwrap();
        assert_eq!(note_server(dir.path(), &server(12)).unwrap(), Seen::First);
    }

    /// A report as macOS writes one: its own header on the first line, then the body.
    fn ips(pid: u64, proc_name: &str, capture: &str) -> String {
        format!(
            "{{\"app_name\":\"tmux\",\"bug_type\":\"309\"}}\n{{\"pid\":{pid},\"procName\":\"{proc_name}\",\"captureTime\":\"{capture}\",\"exception\":{{\"type\":\"EXC_BAD_ACCESS\"}}}}\n"
        )
    }

    const CAPTURE: &str = "2026-10-16 00:12:33.4518 +0200";
    // 2026-10-15 22:12:33 UTC.
    const CAPTURE_UNIX: i64 = 1_792_102_353;

    #[test]
    fn a_capture_time_is_read_as_unix_seconds() {
        assert_eq!(
            parse_capture_time("1970-01-01 00:00:00.0000 +0000"),
            Some(0)
        );
        assert_eq!(
            parse_capture_time("1970-01-02 00:00:00 +0000"),
            Some(86_400)
        );
        assert_eq!(
            parse_capture_time("2026-10-16 00:12:33.4518 +0200"),
            Some(CAPTURE_UNIX)
        );
        assert_eq!(
            parse_capture_time("2026-10-16 00:12:33 +02:00"),
            Some(CAPTURE_UNIX)
        );
        assert_eq!(
            parse_capture_time("2026-10-15 22:12:33 -0000"),
            Some(CAPTURE_UNIX)
        );
        assert_eq!(
            parse_capture_time("2026-10-15 17:12:33.1 -0500"),
            Some(CAPTURE_UNIX)
        );
        assert_eq!(
            parse_capture_time("2026-10-15 22:12:33"),
            Some(CAPTURE_UNIX)
        );
        assert_eq!(
            parse_capture_time("2024-03-01 00:00:00 +0000"),
            Some(1_709_251_200)
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01 00:00:00 +0000",
            "2026-10-16 25:00:00 +0000",
            "2026-10-16 09:12 +0000",
            "2026-10-16 00:12:33 +02",
        ] {
            assert_eq!(parse_capture_time(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_report_is_read_for_its_pid_program_and_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux-2026-10-16-001233.ips");
        fs::write(&path, ips(4242, "tmux", CAPTURE)).unwrap();
        assert_eq!(
            read_report(&path),
            Some(Report {
                path: path.clone(),
                pid: 4242,
                captured_unix: CAPTURE_UNIX
            })
        );
        // Another program, no body, no pid, no capture time, not JSON at all.
        fs::write(&path, ips(4242, "tmux-agent", CAPTURE)).unwrap();
        assert_eq!(read_report(&path), None);
        fs::write(&path, "{\"app_name\":\"tmux\"}\n").unwrap();
        assert_eq!(read_report(&path), None);
        fs::write(
            &path,
            "{}\n{\"procName\":\"tmux\",\"captureTime\":\"2026-10-16 00:12:33 +0200\"}",
        )
        .unwrap();
        assert_eq!(read_report(&path), None);
        fs::write(&path, "{}\n{\"procName\":\"tmux\",\"pid\":1}").unwrap();
        assert_eq!(read_report(&path), None);
        fs::write(&path, "crash\nreport\n").unwrap();
        assert_eq!(read_report(&path), None);
        assert_eq!(read_report(&dir.path().join("none.ips")), None);
    }

    #[test]
    fn only_a_report_of_that_tmux_is_found() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tmux-a.ips"), ips(111, "tmux", CAPTURE)).unwrap();
        fs::write(dir.path().join("tmux-b.ips"), ips(222, "tmux", CAPTURE)).unwrap();
        fs::write(dir.path().join("tmux-c.ips"), ips(333, "other", CAPTURE)).unwrap();
        fs::write(dir.path().join("tmux-d.ips"), "garbage").unwrap();
        // The right pid under a name that is not a tmux report's.
        fs::write(dir.path().join("zsh-e.ips"), ips(444, "tmux", CAPTURE)).unwrap();
        fs::write(dir.path().join("tmux-f.txt"), ips(555, "tmux", CAPTURE)).unwrap();

        let found = find_report(dir.path(), 222, 0).unwrap();
        assert_eq!(found.path, dir.path().join("tmux-b.ips"));
        assert_eq!(
            find_report(dir.path(), 111, 0).unwrap().path,
            dir.path().join("tmux-a.ips")
        );
        for pid in [333, 444, 555, 999] {
            assert_eq!(find_report(dir.path(), pid, 0), None, "{pid}");
        }
        assert_eq!(
            find_report(&dir.path().join("no such folder"), 111, 0),
            None
        );
    }

    #[test]
    fn a_report_from_before_the_server_started_is_not_its_own() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tmux-old.ips"), ips(111, "tmux", CAPTURE)).unwrap();
        assert!(find_report(dir.path(), 111, CAPTURE_UNIX).is_some());
        assert!(find_report(dir.path(), 111, CAPTURE_UNIX + 1).is_none());
    }

    #[test]
    fn the_newest_of_two_reports_for_one_pid_is_found() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tmux-a.ips"),
            ips(111, "tmux", "2026-10-16 00:12:33 +0200"),
        )
        .unwrap();
        fs::write(
            dir.path().join("tmux-b.ips"),
            ips(111, "tmux", "2026-10-16 00:30:00 +0200"),
        )
        .unwrap();
        fs::write(
            dir.path().join("tmux-c.ips"),
            ips(111, "tmux", "2026-10-16 00:20:00 +0200"),
        )
        .unwrap();
        assert_eq!(
            find_report(dir.path(), 111, 0).unwrap().path,
            dir.path().join("tmux-b.ips")
        );
    }

    #[test]
    fn a_report_is_copied_under_its_own_name_and_never_over_a_file() {
        let reports = tempfile::tempdir().unwrap();
        let crashes = tempfile::tempdir().unwrap();
        let path = reports.path().join("tmux-2026-10-16-001233.ips");
        fs::write(&path, ips(4242, "tmux", CAPTURE)).unwrap();
        let report = read_report(&path).unwrap();
        let dir = crashes.path().join("tmux-crashes");

        let kept = keep(&report, &dir).unwrap().unwrap();
        assert_eq!(kept, dir.join("tmux-2026-10-16-001233.ips"));
        assert_eq!(fs::read(&kept).unwrap(), fs::read(&path).unwrap());
        // Nothing but the copy is left in the folder.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        // A second copy finds the name taken and leaves the file as it was.
        fs::write(&kept, "what was there").unwrap();
        assert_eq!(keep(&report, &dir).unwrap(), None);
        assert_eq!(fs::read_to_string(&kept).unwrap(), "what was there");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn the_folder_is_private_and_so_is_each_copy_whatever_the_reports_mode() {
        let reports = tempfile::tempdir().unwrap();
        let crashes = tempfile::tempdir().unwrap();
        let dir = crashes.path().join("tmux-crashes");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        for (name, source_mode) in [("tmux-a.ips", 0o644), ("tmux-b.ips", 0o666)] {
            let path = reports.path().join(name);
            fs::write(&path, ips(4242, "tmux", CAPTURE)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(source_mode)).unwrap();
            let report = read_report(&path).unwrap();
            let kept = keep(&report, &dir).unwrap().unwrap();
            assert_eq!(mode(&kept), 0o600, "{name}");
            assert_eq!(mode(&dir), 0o700, "{name}");
        }
        // A folder that was made wider before is closed again.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = reports.path().join("tmux-c.ips");
        fs::write(&path, ips(4242, "tmux", CAPTURE)).unwrap();
        keep(&read_report(&path).unwrap(), &dir).unwrap().unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn of_two_claims_on_a_name_the_first_wins_and_the_second_loses_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (
            dir.path().join(".first.tmp"),
            dir.path().join(".second.tmp"),
        );
        let dest = dir.path().join("tmux-a.ips");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();

        assert!(link_new(&first, &dest).unwrap());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "first");
        // The second finds the name taken: told so, and the file is as the first left it.
        assert!(!link_new(&second, &dest).unwrap());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "first");
        // A failure that is not "taken" is an error, not a loss.
        assert!(link_new(&dir.path().join("missing"), &dir.path().join("x")).is_err());
    }

    #[test]
    fn the_looks_cover_the_watch_and_start_at_once() {
        assert_eq!(looks(Duration::from_secs(120), Duration::from_secs(2)), 61);
        assert_eq!(looks(Duration::from_secs(0), Duration::from_secs(2)), 1);
        assert_eq!(looks(Duration::from_secs(3), Duration::from_secs(2)), 3);
        assert_eq!(looks(Duration::from_millis(5), Duration::ZERO), 6);
    }

    #[test]
    fn the_toast_names_the_capture_time_and_where_the_report_is() {
        let report = Report {
            path: PathBuf::from("/x/tmux.ips"),
            pid: 1,
            captured_unix: CAPTURE_UNIX,
        };
        let home = Path::new("/Users/someone");
        let kept = home.join(".local/state/tmux-agent-wizard/tmux-crashes/tmux-2026.ips");
        let n = toast(&kept, &report, Some(home), 2 * 3_600);
        assert_eq!(n.title, "tmux crashed at 00:12:33");
        assert_eq!(
            n.body,
            "Its crash report is kept at ~/.local/state/tmux-agent-wizard/tmux-crashes/tmux-2026.ips. taw session restore brings back what it had."
        );
        assert_eq!(n.pane_id, None);
        assert_eq!(n.sound, SoundChoice::None);
        assert_eq!(n.agent, NotifiedAgent::default());
        // At another offset it is another hour; a path outside home stays whole.
        let n = toast(Path::new("/var/state/k.ips"), &report, Some(home), 0);
        assert_eq!(n.title, "tmux crashed at 22:12:33");
        assert!(n.body.contains("kept at /var/state/k.ips."), "{}", n.body);
        assert_eq!(tilde(home, Some(home)), "~");
        assert_eq!(
            tilde(Path::new("/Users/someone2/a"), Some(home)),
            "/Users/someone2/a"
        );
        assert_eq!(tilde(Path::new("/a"), None), "/a");
        assert_eq!(clock(-1, 0), "23:59:59");
    }

    /// What a search did, and what it was told: `appear_at` makes a report on the look of that number
    /// (a look is a sleep before it), and `client_at` attaches a client on the look of that number.
    #[derive(Default)]
    struct Fake {
        sleeps: Vec<Duration>,
        announced: RefCell<Vec<Notification>>,
        reports_dir: PathBuf,
        appear_at: Option<usize>,
        appear_pid: u32,
        client_at: Option<usize>,
        asked_for_client: usize,
    }

    impl Effects for Fake {
        fn sleep(&mut self, how_long: Duration) {
            self.sleeps.push(how_long);
            if self.appear_at == Some(self.sleeps.len()) {
                fs::write(
                    self.reports_dir.join("tmux-late.ips"),
                    ips(u64::from(self.appear_pid), "tmux", CAPTURE),
                )
                .unwrap();
            }
        }
        fn client_attached(&mut self) -> bool {
            self.asked_for_client += 1;
            self.client_at.is_some_and(|at| self.asked_for_client > at)
        }
        fn announce(&mut self, notification: &Notification) {
            self.announced.borrow_mut().push(notification.clone());
        }
        fn utc_offset(&self, _unix: i64) -> i64 {
            2 * 3_600
        }
    }

    struct World {
        reports: tempfile::TempDir,
        crashes: tempfile::TempDir,
    }

    impl World {
        fn new() -> World {
            World {
                reports: tempfile::tempdir().unwrap(),
                crashes: tempfile::tempdir().unwrap(),
            }
        }
        fn fake(&self) -> Fake {
            Fake {
                reports_dir: self.reports.path().to_path_buf(),
                appear_pid: 4242,
                ..Fake::default()
            }
        }
        fn search(&self, needs_client: bool) -> Search<'_> {
            Search {
                reports_dir: self.reports.path(),
                crashes_dir: self.crashes.path(),
                home: None,
                watch: Duration::from_secs(10),
                poll: Duration::from_secs(2),
                client_wait: Duration::from_secs(6),
                needs_client,
            }
        }
        fn copies(&self) -> usize {
            fs::read_dir(self.crashes.path()).unwrap().count()
        }
    }

    const LAST: Record = Record {
        pid: 4242,
        start_time: 1_000,
    };

    #[test]
    fn a_matching_report_is_kept_and_announced_once() {
        let world = World::new();
        fs::write(
            world.reports.path().join("tmux-a.ips"),
            ips(4242, "tmux", CAPTURE),
        )
        .unwrap();
        let mut fake = world.fake();
        let found = keep_report_of(&LAST, &world.search(false), &mut fake);
        assert!(found.announced);
        assert_eq!(found.kept, Some(world.crashes.path().join("tmux-a.ips")));
        assert!(fake.sleeps.is_empty(), "the first look is at once");
        let told = fake.announced.borrow();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].title, "tmux crashed at 00:12:33");

        // A second run at the same moment finds the copy there, and says nothing.
        let mut second = world.fake();
        let found = keep_report_of(&LAST, &world.search(false), &mut second);
        assert_eq!(found, Found::default());
        assert!(second.announced.borrow().is_empty());
        assert_eq!(world.copies(), 1);
    }

    #[test]
    fn a_report_of_another_pid_or_program_or_one_that_does_not_parse_is_not_kept() {
        let world = World::new();
        let reports = world.reports.path();
        fs::write(reports.join("tmux-a.ips"), ips(1, "tmux", CAPTURE)).unwrap();
        fs::write(reports.join("tmux-b.ips"), ips(4242, "node", CAPTURE)).unwrap();
        fs::write(reports.join("tmux-c.ips"), "{{{").unwrap();
        let mut fake = world.fake();
        let found = keep_report_of(&LAST, &world.search(false), &mut fake);
        assert_eq!(found, Found::default());
        assert!(fake.announced.borrow().is_empty());
        assert_eq!(world.copies(), 0);
        // It looked for the whole watch: ten seconds, two apart.
        assert_eq!(fake.sleeps, vec![Duration::from_secs(2); 5]);
    }

    #[test]
    fn a_report_that_appears_after_a_few_looks_is_found() {
        let world = World::new();
        let mut fake = world.fake();
        fake.appear_at = Some(3);
        let found = keep_report_of(&LAST, &world.search(false), &mut fake);
        assert!(found.announced, "{found:?}");
        assert_eq!(fake.sleeps.len(), 3, "it stopped looking once it had it");
        assert_eq!(world.copies(), 1);

        // One that comes after the watch is over is not looked for.
        let world = World::new();
        let mut fake = world.fake();
        fake.appear_at = Some(6);
        let found = keep_report_of(&LAST, &world.search(false), &mut fake);
        assert_eq!(found, Found::default());
        assert_eq!(world.copies(), 0);
    }

    #[test]
    fn a_report_from_before_the_last_server_started_is_not_kept() {
        let world = World::new();
        fs::write(
            world.reports.path().join("tmux-a.ips"),
            ips(4242, "tmux", CAPTURE),
        )
        .unwrap();
        let later = Record {
            pid: 4242,
            start_time: CAPTURE_UNIX as u64 + 60,
        };
        let mut fake = world.fake();
        assert_eq!(
            keep_report_of(&later, &world.search(false), &mut fake),
            Found::default()
        );
        assert_eq!(world.copies(), 0);
    }

    #[test]
    fn a_toast_that_needs_a_client_waits_for_one_and_the_copy_stays_without() {
        let world = World::new();
        fs::write(
            world.reports.path().join("tmux-a.ips"),
            ips(4242, "tmux", CAPTURE),
        )
        .unwrap();
        let mut fake = world.fake();
        fake.client_at = Some(2);
        let found = keep_report_of(&LAST, &world.search(true), &mut fake);
        assert!(found.announced);
        assert_eq!(fake.asked_for_client, 3);
        assert_eq!(fake.sleeps.len(), 2);

        // Nobody attaches within the wait: no toast, and the copy stays.
        let world = World::new();
        fs::write(
            world.reports.path().join("tmux-a.ips"),
            ips(4242, "tmux", CAPTURE),
        )
        .unwrap();
        let mut fake = world.fake();
        let found = keep_report_of(&LAST, &world.search(true), &mut fake);
        assert!(!found.announced);
        assert_eq!(found.kept, Some(world.crashes.path().join("tmux-a.ips")));
        assert!(fake.announced.borrow().is_empty());
        assert_eq!(world.copies(), 1);
        assert_eq!(
            fake.asked_for_client, 4,
            "6 s of waiting, 2 s apart, the first look at once"
        );

        // A toast that does not need one never asks.
        let world = World::new();
        fs::write(
            world.reports.path().join("tmux-a.ips"),
            ips(4242, "tmux", CAPTURE),
        )
        .unwrap();
        let mut fake = world.fake();
        keep_report_of(&LAST, &world.search(false), &mut fake);
        assert_eq!(fake.asked_for_client, 0);
    }
}
