//! What a plugin's `[[startup]]` commands left running, so that `disable` and `uninstall` can end it.
//!
//! Each startup command is started as the leader of a process group of its own, and its pid, the
//! time the system says it started, the boot it started in and the time taw started it (on Linux
//! counted from the boot, which a step of the wall clock does not move) go in
//! `<state>/<id>/startup.pids`. Ending a plugin's processes signals those groups, only when the
//! group is proven to be the one recorded:
//!
//! - A record from an earlier boot is ignored: its pid means nothing now.
//! - A leader that is the process recorded (the same start time) proves its group.
//! - A pid that is another process now (a start time that differs) is left alone: the system does
//!   not give a pid to another process while a group still bears it, so that group is gone.
//! - A group outlives its leader while any member runs, and a startup command that puts its work in
//!   the background (`cmd &`, `nohup cmd &`, the usual shape of a daemon) exits at once. A group
//!   whose leader is gone is signalled only when one of its members started within
//!   [`STARTED_WITH_SECS`] of the time taw started the command. Once the group was wholly gone the
//!   system can give its id to a new group (the first command of a shell pipeline, `cat log | less`,
//!   that ends before the rest), and every process of that one started after the first group was
//!   gone, so it is left alone and the log says so.
//!
//! A start time, not a token in the environment, is the proof for a group without its leader: macOS
//! no longer shows another process's environment (`KERN_PROCARGS2` and `ps -E` give the arguments
//! only), and how long ago a process started (`ps -o etime=`) reads the same on macOS and Linux,
//! whatever the locale or time zone. Nothing is ever found by name.
//!
//! When startup runs, the records proven gone (the leader is not the process recorded and the
//! group has no members) and those from an earlier boot are dropped before new ones are added, so
//! the file does not grow across server starts and reboots.

/// What a record holds for a start time it could not read: the command had already exited when taw
/// asked, so only its group, if it still has members, tells that it is there.
const UNREAD: &str = "-";

/// How long after taw started a startup command a process of its group can have started and still
/// prove, once the command itself has exited, that the group is the plugin's. A group that got the
/// id again was started after the first one was wholly gone, and the system hands pids out in turn,
/// so that takes a whole round of the pid space (99999 on macOS) inside these few seconds.
const STARTED_WITH_SECS: u64 = 10;

use std::io::Write as _;

use super::PluginCtx;

fn pid_file(ctx: &PluginCtx, id: &str) -> std::path::PathBuf {
    ctx.paths.plugin_state_dir.join(id).join("startup.pids")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `ps`, made to print times the same way whatever the locale and time zone of whoever runs taw: a
/// start time recorded by the tmux server's startup is compared with one read from a user's shell.
fn ps() -> std::process::Command {
    pinned(std::process::Command::new("ps"))
}

fn pinned(mut command: std::process::Command) -> std::process::Command {
    command.env("LC_ALL", "C").env("TZ", "UTC0");
    command
}

/// When the system says the process `pid` started, `None` for a process that is not there: a value
/// the system keeps from the moment of the fork, which a step of the wall clock does not move. On
/// Linux that is field 22 of `/proc/<pid>/stat`, the clock ticks from the boot to the start (`ps
/// lstart` there is worked out from the boot time, which a clock step moves). Elsewhere it is `ps -o
/// lstart=`, which on macOS prints the time the kernel stored at the fork.
#[cfg(target_os = "linux")]
fn started_at(pid: u32) -> Option<String> {
    started_at_in(std::path::Path::new("/proc"), pid)
}

#[cfg(not(target_os = "linux"))]
fn started_at(pid: u32) -> Option<String> {
    started_at_by(pid, ps())
}

/// Field 22 of `<proc>/<pid>/stat` ([`start_ticks`]).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn started_at_in(proc: &std::path::Path, pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    start_ticks(&stat)
}

/// The start time in a line of `/proc/<pid>/stat`: its field 22, counted after the last `)`, since
/// the command name in field 2 is in parentheses and can hold spaces and parentheses of its own.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn start_ticks(stat: &str) -> Option<String> {
    let (_, after_name) = stat.rsplit_once(')')?;
    // The words after the name start at field 3.
    let ticks = after_name.split_whitespace().nth(22 - 3)?;
    ticks.parse::<u64>().ok()?;
    Some(ticks.to_string())
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
fn started_at_by(pid: u32, mut ps: std::process::Command) -> Option<String> {
    let out = ps
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// How many seconds `ps -o etime=` says have passed: `[[dd-]hh:]mm:ss`.
fn parse_elapsed(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let parts = clock
        .split(':')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let (hours, minutes, seconds) = match parts[..] {
        [minutes, seconds] => (0, minutes, seconds),
        [hours, minutes, seconds] => (hours, minutes, seconds),
        _ => return None,
    };
    Some(((days * 24 + hours) * 60 + minutes) * 60 + seconds)
}

/// The processes in the process group `group`, each with how many seconds ago it started
/// (`ps -A -o pid=,pgid=,etime=`), none when `ps` fails.
fn group_members(group: u32) -> Vec<(u32, u64)> {
    let Ok(out) = ps()
        .args(["-A", "-o", "pid=,pgid=,etime="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse::<u32>().ok()?;
            let pgid = words.next()?.parse::<u32>().ok()?;
            let elapsed = parse_elapsed(words.next()?)?;
            (pgid == group).then_some((pid, elapsed))
        })
        .collect()
}

/// Whether any process of the system is in the process group `group`.
fn group_has_members(group: u32) -> bool {
    !group_members(group).is_empty()
}

/// Whether a process that started at `start` (seconds on [`GROUP_CLOCK`]) started with a command
/// taw started at `spawned`: no more than [`STARTED_WITH_SECS`] after it, and not before it but for
/// the second `ps` rounds away on each side.
fn started_with(start: u64, spawned: u64) -> bool {
    start + 2 >= spawned && start <= spawned + STARTED_WITH_SECS
}

/// A clock the time taw started a startup command is kept on, and the members of a group whose
/// command has exited are checked against.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Clock {
    /// Seconds since 1970.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Wall,
    /// Seconds since the boot, sleep included.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Boot,
}

/// Readings of both clocks at one moment.
#[derive(Clone, Copy, Debug)]
struct Now {
    wall: u64,
    boot: u64,
}

/// The clock a group whose command has exited is checked on: the one `ps -o etime=` is worked out
/// from, so that a step of the wall clock between the record and the check moves neither or both.
/// procps on Linux takes `etime` from the uptime, which a step does not move; macOS takes it from
/// the wall clock less the start the kernel stored at the fork, which a step moves as much as `now`.
#[cfg(target_os = "linux")]
const GROUP_CLOCK: Clock = Clock::Boot;
#[cfg(not(target_os = "linux"))]
const GROUP_CLOCK: Clock = Clock::Wall;

impl Now {
    fn read() -> Now {
        Now {
            wall: now_secs(),
            boot: boot_secs(),
        }
    }

    fn on(self, clock: Clock) -> u64 {
        match clock {
            Clock::Wall => self.wall,
            Clock::Boot => self.boot,
        }
    }
}

/// Seconds since the boot, sleep included: `CLOCK_BOOTTIME` on Linux, which `/proc/uptime` reads.
/// (Elsewhere `CLOCK_MONOTONIC`, which [`GROUP_CLOCK`] does not use there.)
fn boot_secs() -> u64 {
    #[cfg(target_os = "linux")]
    let clock = libc::CLOCK_BOOTTIME;
    #[cfg(not(target_os = "linux"))]
    let clock = libc::CLOCK_MONOTONIC;
    // SAFETY: an all-zero `timespec` is a valid value of the plain C struct.
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `time` is a `timespec` this function owns, the one place the call writes.
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return 0;
    }
    u64::try_from(time.tv_sec).unwrap_or(0)
}

/// Whether a member of a group that `ps -o etime=` says started `elapsed` seconds before `now`
/// started with a command taw started at `spawned`, read on `clock`.
fn member_started_with(clock: Clock, now: Now, elapsed: u64, spawned: u64) -> bool {
    started_with(now.on(clock).saturating_sub(elapsed), spawned)
}

/// Which boot this is: `kern.bootsessionuuid` on macOS, `/proc/sys/kernel/random/boot_id` on Linux,
/// each an id made once per boot, `None` where it cannot be read. (Not `kern.boottime`: the system
/// moves it when the wall clock is stepped, and every record would then look like an earlier
/// boot's. A record that names its boot so, an older taw's, is taken for an earlier boot.)
#[cfg(target_os = "macos")]
fn boot_id() -> Option<String> {
    let mut id = [0u8; 64];
    let mut size = id.len();
    // SAFETY: the name is a NUL-terminated string, and `id` is a buffer of `size` bytes that the
    // call is told about, so it writes no more than that.
    let asked = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            id.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if asked != 0 {
        return None;
    }
    let id = id[..size.min(id.len())].split(|&byte| byte == 0).next()?;
    let id = std::str::from_utf8(id).ok()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_id() -> Option<String> {
    None
}

/// One line of `startup.pids`: `pid \t start time \t boot \t when taw started it`, the last in
/// seconds on [`GROUP_CLOCK`]. A line written before the boot and the time were kept has neither.
struct Record<'a> {
    pid: u32,
    started: &'a str,
    boot: Option<&'a str>,
    spawned: Option<u64>,
}

impl<'a> Record<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split('\t');
        let pid = fields.next()?.parse::<u32>().ok().filter(|pid| *pid != 0)?;
        let started = fields.next()?;
        let boot = fields.next().filter(|boot| !boot.is_empty());
        let spawned = fields.next().and_then(|secs| secs.parse::<u64>().ok());
        Some(Record {
            pid,
            started,
            boot,
            spawned,
        })
    }

    /// Whether the record was written in this boot, which `boot` names. A record that does not say,
    /// or a boot that cannot be read, is not taken for this one.
    fn of_this_boot(&self, boot: Option<&str>) -> bool {
        boot.is_some() && self.boot == boot
    }

    /// Whether the leader recorded is still the process with this pid.
    fn leader_is_there(&self) -> bool {
        started_at(self.pid).is_some_and(|now| now == self.started)
    }
}

/// Records a startup command of plugin `id` that was started a moment ago as a process group leader.
pub fn record(ctx: &PluginCtx, id: &str, pid: u32) {
    let spawned = Now::read().on(GROUP_CLOCK);
    let started = started_at(pid).unwrap_or_else(|| UNREAD.to_string());
    let boot = boot_id().unwrap_or_default();
    let path = pid_file(ctx, id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = append_record(&path, &format!("{pid}\t{started}\t{boot}\t{spawned}")) {
        log::warn!("plugin '{id}': could not record startup pid {pid}, {err}");
    }
}

/// Adds `line` at the end of the records file at `path`, made when there is none, with the file's
/// lock held ([`lock_records`]).
fn append_record(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    let mut file = lock_records(path, true)?;
    writeln!(file, "{line}")
}

/// Opens the records file at `path` with its lock held (`flock`, let go when the file is closed),
/// so that an append, a rewrite and `stop` never run at once: a startup that adds its record while
/// another drops the ended ones keeps it. A rewrite puts a new file in place, so a lock taken on the
/// file it replaced is let go and taken again on the one now at `path`. With `create`, a missing
/// file is made; without it, a missing file is an error of kind `NotFound`. Where the file system
/// keeps no locks ([`keeps_no_locks`]) the file is opened without one, as before there was a lock,
/// and taw's log says so.
fn lock_records(path: &std::path::Path, create: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt as _;
    loop {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(create)
            .open(path)?;
        if let Err(err) = lock_exclusive(&file) {
            if !keeps_no_locks(&err) {
                return Err(err);
            }
            log::warn!(
                "{}: used without a lock, since its file system keeps none ({err})",
                path.display()
            );
            return Ok(file);
        }
        let held = file.metadata()?;
        match std::fs::metadata(path) {
            Ok(now) if now.dev() == held.dev() && now.ino() == held.ino() => return Ok(file),
            // Replaced or removed while this waited for the lock: open what is there now.
            Ok(_) => continue,
            Err(err) if create && err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The error `flock` gives in this thread instead of the lock, while a test stands for a file
    /// system that keeps no locks.
    static LOCK_FAILS_WITH: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// Takes the lock of `file`, waiting for it (`flock`), again when a signal cut the wait short.
fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    #[cfg(test)]
    if let Some(errno) = LOCK_FAILS_WITH.get() {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    // SAFETY: `flock` takes the descriptor of a file the caller holds open and an integer, and
    // touches no memory of this process.
    while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(())
}

/// Whether the lock failed because the file system keeps no locks (some network and FUSE mounts),
/// not because the lock could not be had.
fn keeps_no_locks(err: &std::io::Error) -> bool {
    err.raw_os_error().is_some_and(|errno| {
        [libc::ENOLCK, libc::EOPNOTSUPP, libc::ENOTSUP, libc::ENOSYS].contains(&errno)
    })
}

/// Drops plugin `id`'s records that name nothing any more: those from an earlier boot, and those
/// whose leader is not the process recorded and whose group has no members. Run when startup runs,
/// before the new records are added.
pub fn forget_gone(ctx: &PluginCtx, id: &str) {
    let path = pid_file(ctx, id);
    let boot = boot_id();
    let kept = keep_records(&path, |line| {
        Record::parse(line).is_some_and(|record| {
            record.of_this_boot(boot.as_deref())
                && (record.leader_is_there() || group_has_members(record.pid))
        })
    });
    if let Err(err) = kept {
        log::warn!("plugin '{id}': could not drop its ended startup records, {err}");
    }
}

/// Keeps the lines of the records file at `path` that `keep` says to keep and drops the others,
/// removing the file when none is left. A missing file is left missing. The kept lines go to a
/// new file that is renamed into place, all with the file's lock held ([`lock_records`]), so an
/// append made meanwhile waits and then lands in the file at `path`.
fn keep_records(path: &std::path::Path, mut keep: impl FnMut(&str) -> bool) -> std::io::Result<()> {
    use std::io::Read as _;
    let mut file = match lock_records(path, false) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let kept: String = text
        .lines()
        .filter(|line| keep(line))
        .map(|line| format!("{line}\n"))
        .collect();
    let done = if kept.is_empty() {
        std::fs::remove_file(path)
    } else {
        let mut new = path.as_os_str().to_owned();
        new.push(".new");
        let new = std::path::PathBuf::from(new);
        let wrote = std::fs::write(&new, kept).and_then(|()| std::fs::rename(&new, path));
        if wrote.is_err() {
            let _ = std::fs::remove_file(&new);
        }
        wrote
    };
    // The lock is let go only once the file at `path` is the new one.
    drop(file);
    done
}

/// Ends what plugin `id`'s startup commands started that is still running, and forgets the record.
/// Returns how many process groups it signalled.
pub fn stop(ctx: &PluginCtx, id: &str) -> usize {
    use std::io::Read as _;
    let path = pid_file(ctx, id);
    // Held until the file is removed, so a record a startup adds meanwhile is not removed unread.
    let Ok(mut file) = lock_records(&path, false) else {
        return 0;
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return 0;
    }
    let boot = boot_id();
    let mut signalled = 0;
    for record in text.lines().filter_map(Record::parse) {
        // A pid of an earlier boot names nothing of ours.
        if !record.of_this_boot(boot.as_deref()) {
            continue;
        }
        let pid = record.pid;
        match started_at(pid) {
            Some(now) if now == record.started => {}
            // The pid is another process now, so the recorded group is gone.
            Some(_) => continue,
            // The leader has exited: the group is ours when a member started with the command.
            None => {
                let members = group_members(pid);
                if members.is_empty() {
                    continue;
                }
                let now = Now::read();
                let proven = record.spawned.is_some_and(|spawned| {
                    members.iter().any(|&(_, elapsed)| {
                        member_started_with(GROUP_CLOCK, now, elapsed, spawned)
                    })
                });
                if !proven {
                    log::warn!(
                        "plugin '{id}': left process group {pid} alone: its startup command has exited and nothing in the group started with it"
                    );
                    continue;
                }
            }
        }
        let Ok(group) = libc::pid_t::try_from(pid) else {
            continue;
        };
        // SAFETY: `killpg` takes two integers and touches no memory of this process. The group is the
        // one the recorded process leads: it was started with `process_group(0)`.
        if unsafe { libc::killpg(group, libc::SIGTERM) } == 0 {
            signalled += 1;
        }
    }
    let _ = std::fs::remove_file(&path);
    drop(file);
    signalled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::tests_support::test_paths;

    /// The time a record made now keeps for when taw started its command.
    fn spawned_now() -> u64 {
        Now::read().on(GROUP_CLOCK)
    }

    fn ctx_parts() -> (
        tempfile::TempDir,
        crate::paths::Paths,
        crate::config::Config,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        (dir, paths, crate::config::Config::default())
    }

    #[test]
    fn stop_ends_the_recorded_group_and_leaves_a_pid_the_system_gave_to_another_process() {
        let (dir, paths, cfg) = ctx_parts();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-running-test-unused");
        let bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &bin,
            server_env: None,
        };
        let log = dir.path().join("log");
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 30 & wait".to_string(),
        ];
        let pid =
            crate::plugin::env::spawn_detached_group(dir.path(), &command, &[], &log).unwrap();
        // The guard ends the group whatever the assertions do.
        struct Guard(libc::pid_t);
        impl Drop for Guard {
            fn drop(&mut self) {
                // SAFETY: two integers, no memory.
                unsafe { libc::killpg(self.0, libc::SIGKILL) };
            }
        }
        let _guard = Guard(pid as libc::pid_t);
        record(&ctx, "p", pid);

        // A record of this boot whose start time is not the process's: left alone.
        let stale = pid_file(&ctx, "q");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        let boot = boot_id().unwrap();
        std::fs::write(
            &stale,
            format!(
                "{pid}\tMon Jan  1 00:00:00 1990\t{boot}\t{}\n",
                spawned_now()
            ),
        )
        .unwrap();
        assert_eq!(stop(&ctx, "q"), 0);
        assert!(started_at(pid).is_some(), "an unrecorded process was ended");

        assert_eq!(stop(&ctx, "p"), 1);
        // Gone, or a zombie this test process has not reaped (it started the process itself).
        let ended = || {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .map(|out| {
                    let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    stat.is_empty() || stat.starts_with('Z')
                })
                .unwrap_or(false)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !ended() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(ended(), "the recorded group was not ended");
        assert!(!pid_file(&ctx, "p").exists());
    }

    /// Ends the process group `group` with SIGKILL when dropped, whatever the test's assertions did.
    struct Guard(libc::pid_t);
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: two integers, no memory.
            unsafe { libc::killpg(self.0, libc::SIGKILL) };
        }
    }

    /// Starts `script` as the leader of a group of its own, the way a startup command is started.
    /// A test whose leader must be the sleep itself says `exec sleep 30`: dash, Debian's and Ubuntu's
    /// `sh`, stays the parent of a single command where bash execs it.
    fn start_group(script: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        std::process::Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_until_group_ends(group: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while group_has_members(group) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!group_has_members(group), "the group {group} was not ended");
    }

    fn plugin_ctx<'a>(
        tmux: &'a crate::tmux::Tmux,
        cfg: &'a crate::config::Config,
        paths: &'a crate::paths::Paths,
        bin: &'a std::path::PathBuf,
    ) -> PluginCtx<'a> {
        PluginCtx {
            tmux,
            cfg,
            paths,
            taw_bin: bin,
            server_env: None,
        }
    }

    /// The usual daemon shape: the startup command backgrounds its work and exits, leaving a group with
    /// no leader. Recorded after the leader ended (taw asks the system a moment after the start, and
    /// a command that only backgrounds something is gone by then) or before it, `stop` ends the group.
    #[test]
    fn stop_ends_the_group_of_a_startup_command_that_backgrounded_its_work_and_exited() {
        let (_dir, paths, cfg) = ctx_parts();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-running-test-unused");
        let bin = std::path::PathBuf::from("/bin/true");
        let ctx = plugin_ctx(&tmux, &cfg, &paths, &bin);

        // Recorded after the leader exited and was reaped: the start time cannot be read.
        let mut gone = start_group("sleep 30 &");
        let gone_pid = gone.id();
        let _gone_guard = Guard(gone_pid as libc::pid_t);
        gone.wait().unwrap();
        assert!(started_at(gone_pid).is_none(), "the leader is still there");
        record(&ctx, "late", gone_pid);
        assert!(
            group_has_members(gone_pid),
            "the sleeper is not in the group"
        );
        assert_eq!(stop(&ctx, "late"), 1);
        wait_until_group_ends(gone_pid);

        // Recorded while the leader ran, then the leader ended and the sleeper stayed.
        let mut early = start_group("sleep 30 & sleep 30");
        let early_pid = early.id();
        let _early_guard = Guard(early_pid as libc::pid_t);
        record(&ctx, "early", early_pid);
        assert!(
            !std::fs::read_to_string(pid_file(&ctx, "early"))
                .unwrap()
                .contains("\t-\n")
        );
        // The shell has started the background sleeper before its leader is killed (under load it
        // can still be on its way there).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while group_members(early_pid).len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            group_members(early_pid).len() >= 2,
            "the background sleeper did not start: {:?}",
            group_members(early_pid)
        );
        // SAFETY: a signal to the one child this test started.
        unsafe { libc::kill(early_pid as libc::pid_t, libc::SIGKILL) };
        early.wait().unwrap();
        assert!(group_has_members(early_pid));
        assert_eq!(stop(&ctx, "early"), 1);
        wait_until_group_ends(early_pid);
    }

    /// A record that names no process and no group, or a pid that is another process now, is left alone.
    #[test]
    fn stop_leaves_a_record_with_no_group_and_a_pid_that_is_another_process_now() {
        let (_dir, paths, cfg) = ctx_parts();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-running-test-unused");
        let bin = std::path::PathBuf::from("/bin/true");
        let ctx = plugin_ctx(&tmux, &cfg, &paths, &bin);

        // A group that ended: its pid, read and reaped, has no members.
        let mut ended = start_group("true");
        let ended_pid = ended.id();
        ended.wait().unwrap();
        record(&ctx, "ended", ended_pid);
        assert!(!group_has_members(ended_pid));
        assert_eq!(stop(&ctx, "ended"), 0);

        // A pid that is alive but is not the recorded process, whatever its start time.
        let mut other = start_group("exec sleep 30");
        let other_pid = other.id();
        let _guard = Guard(other_pid as libc::pid_t);
        let path = pid_file(&ctx, "other");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let boot = boot_id().unwrap();
        std::fs::write(
            &path,
            format!("{other_pid}\t-\t{boot}\t{}\n", spawned_now()),
        )
        .unwrap();
        assert_eq!(stop(&ctx, "other"), 0);
        assert!(started_at(other_pid).is_some(), "a live process was ended");
        // SAFETY: a signal to the one child this test started.
        unsafe { libc::kill(other_pid as libc::pid_t, libc::SIGKILL) };
        other.wait().unwrap();
    }

    /// Waits until the leader of the group `group` has gone while the group still has members, the
    /// shape of a startup command that has exited and left its work running.
    fn wait_for_leaderless_group(group: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !(started_at(group).is_none() && group_has_members(group))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            started_at(group).is_none(),
            "the leader of {group} is still there"
        );
        assert!(group_has_members(group), "the group {group} has no members");
    }

    /// A record that names a group whose leader has exited and none of whose processes started with
    /// the recorded command (a group taw did not start that got a recorded id, like a shell pipeline
    /// whose first command ended, long after the record was written) is left alone.
    #[test]
    fn stop_leaves_a_leaderless_group_whose_processes_did_not_start_with_the_recorded_command() {
        let (_dir, paths, cfg) = ctx_parts();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-running-test-unused");
        let bin = std::path::PathBuf::from("/bin/true");
        let ctx = plugin_ctx(&tmux, &cfg, &paths, &bin);

        let mut stranger = start_group("sleep 30 &");
        let pid = stranger.id();
        let _guard = Guard(pid as libc::pid_t);
        stranger.wait().unwrap();
        wait_for_leaderless_group(pid);

        let boot = boot_id().unwrap();
        let path = pid_file(&ctx, "reused");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Recorded an hour ago, a minute ago, and with no time at all (a line of an older taw).
        for spawned in [
            spawned_now().saturating_sub(3600).to_string(),
            spawned_now().saturating_sub(60).to_string(),
            String::new(),
        ] {
            std::fs::write(&path, format!("{pid}\t-\t{boot}\t{spawned}\n")).unwrap();
            assert_eq!(stop(&ctx, "reused"), 0, "recorded at {spawned:?}");
            assert!(!path.exists());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            group_has_members(pid),
            "a group taw did not start was ended"
        );
    }

    /// A record from an earlier boot is ignored by `stop`, whatever its pid is now, and startup drops
    /// it with the records whose group has ended, keeping those that still run.
    #[test]
    fn a_record_from_an_earlier_boot_is_ignored_and_startup_drops_it_with_the_ended_ones() {
        let (_dir, paths, cfg) = ctx_parts();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-running-test-unused");
        let bin = std::path::PathBuf::from("/bin/true");
        let ctx = plugin_ctx(&tmux, &cfg, &paths, &bin);
        let boot = boot_id().unwrap();

        // A live leader with its own start time, recorded in another boot.
        let mut live = start_group("exec sleep 30");
        let live_pid = live.id();
        let _guard = Guard(live_pid as libc::pid_t);
        record(&ctx, "earlier", live_pid);
        let path = pid_file(&ctx, "earlier");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains(&format!("\t{boot}\t")), "{written}");
        std::fs::write(&path, written.replace(&boot, "an-earlier-boot")).unwrap();
        assert_eq!(stop(&ctx, "earlier"), 0);
        assert!(
            started_at(live_pid).is_some(),
            "a record of an earlier boot ended a process"
        );

        // Startup drops the earlier boot's record, the ended group's and an older taw's line, and
        // keeps the live one.
        let mut ended = start_group("true");
        let ended_pid = ended.id();
        ended.wait().unwrap();
        let path = pid_file(&ctx, "mixed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let now = spawned_now();
        let live_line = format!(
            "{live_pid}\t{}\t{boot}\t{now}",
            started_at(live_pid).unwrap()
        );
        std::fs::write(
            &path,
            format!(
                "{live_pid}\t-\tan-earlier-boot\t{now}\n{ended_pid}\t-\t{boot}\t{now}\n{live_line}\n{live_pid}\tnot a time\n"
            ),
        )
        .unwrap();
        forget_gone(&ctx, "mixed");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{live_line}\n")
        );

        // SAFETY: a signal to the one child this test started.
        unsafe { libc::kill(live_pid as libc::pid_t, libc::SIGKILL) };
        live.wait().unwrap();
        forget_gone(&ctx, "mixed");
        assert!(!path.exists(), "a file of ended records was kept");
    }

    /// On macOS the boot is named by its boot session UUID, which a step of the wall clock does not
    /// move, and a record that names it by the boot time (an older taw's) is not of this boot.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_boot_is_named_by_its_boot_session_uuid_on_macos() {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .unwrap();
        let uuid = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert_eq!(uuid.len(), 36, "{uuid:?}");
        assert_eq!(boot_id().as_deref(), Some(uuid.as_str()));
        let old = Record::parse("123\t-\t1761000000.123456\t1761000100").unwrap();
        assert!(!old.of_this_boot(boot_id().as_deref()));
    }

    /// On Linux the start time is field 22 of `/proc/<pid>/stat`, counted after the last `)` of the
    /// command name, which can hold spaces and parentheses.
    #[test]
    fn a_start_time_is_field_22_of_proc_stat_counted_after_the_command_name() {
        let after = "R 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 12345678 300";
        assert_eq!(
            start_ticks(&format!("4242 (sleep) {after}\n")).as_deref(),
            Some("987654")
        );
        assert_eq!(
            start_ticks(&format!("4242 (my ) (prog) 1 2 3) {after}\n")).as_deref(),
            Some("987654")
        );
        assert_eq!(start_ticks("4242 (sleep) R 1 4242"), None);
        assert_eq!(start_ticks(&format!("4242 sleep {after}")), None);
        assert_eq!(
            start_ticks(
                "4242 (sleep) R 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 soon 1"
            ),
            None
        );

        let proc = tempfile::tempdir().unwrap();
        std::fs::create_dir(proc.path().join("4242")).unwrap();
        std::fs::write(
            proc.path().join("4242/stat"),
            format!("4242 (a ) (b) {after}\n"),
        )
        .unwrap();
        assert_eq!(started_at_in(proc.path(), 4242).as_deref(), Some("987654"));
        assert_eq!(started_at_in(proc.path(), 4243), None);
    }

    /// A startup that adds its record while another startup drops the ended ones (a second tmux
    /// server, a reload of tmux.conf) keeps it: the append waits for the rewrite, and the rewrite
    /// does not write over it.
    #[test]
    fn a_record_added_while_startup_drops_the_ended_ones_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup.pids");
        std::fs::write(&path, "1\tlive\n2\tended\n").unwrap();
        let (asked, appending) = std::sync::mpsc::channel();
        let mut appender = None;
        keep_records(&path, |line| {
            if appender.is_none() {
                let path = path.clone();
                let asked = asked.clone();
                appender = Some(std::thread::spawn(move || {
                    asked.send(()).unwrap();
                    append_record(&path, "3\tnew").unwrap();
                }));
                appending
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .unwrap();
                // The append is made now, or waits for the rewrite to let go of the file.
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            line.starts_with("1\t")
        })
        .unwrap();
        appender.unwrap().join().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\tlive\n3\tnew\n");

        // When every record is dropped, the append still lands, in a file of its own.
        std::fs::write(&path, "2\tended\n").unwrap();
        let mut appender = None;
        keep_records(&path, |_| {
            if appender.is_none() {
                let path = path.clone();
                let asked = asked.clone();
                appender = Some(std::thread::spawn(move || {
                    asked.send(()).unwrap();
                    append_record(&path, "4\tnewer").unwrap();
                }));
                appending
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            false
        })
        .unwrap();
        appender.unwrap().join().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4\tnewer\n");
    }

    /// A group whose command has exited is proven by when its members started, on a clock that a
    /// step of the wall clock moves together with `ps -o etime=`: on Linux the time since the boot
    /// (procps works `etime` out from the uptime), on macOS the wall clock (`etime` there is the wall
    /// clock less the start the kernel stored at the fork).
    #[test]
    fn a_leaderless_group_is_proven_across_a_step_of_the_wall_clock() {
        // taw starts the command, and a member of its group starts a second later.
        let at_record = Now {
            wall: 1_761_000_000,
            boot: 5_000,
        };
        let member_wall = at_record.wall + 1;
        let member_boot = at_record.boot + 1;
        // A minute later, after the wall clock was stepped an hour ahead or back (a Linux VM once
        // its host woke up).
        for step in [3600, -3600_i64] {
            let at_check = Now {
                wall: at_record.wall.checked_add_signed(60 + step).unwrap(),
                boot: at_record.boot + 60,
            };
            let linux_elapsed = at_check.boot - member_boot;
            let spawned = at_record.on(Clock::Boot);
            assert!(
                member_started_with(Clock::Boot, at_check, linux_elapsed, spawned),
                "a step of {step} s hid the group on Linux"
            );
            // A member that started half a minute after the command did not start with it.
            assert!(!member_started_with(
                Clock::Boot,
                at_check,
                linux_elapsed - 30,
                spawned
            ));
        }
        let at_check = Now {
            wall: at_record.wall + 60 + 3600,
            boot: at_record.boot + 60,
        };
        let mac_elapsed = at_check.wall - member_wall;
        let spawned = at_record.on(Clock::Wall);
        assert!(member_started_with(
            Clock::Wall,
            at_check,
            mac_elapsed,
            spawned
        ));
        assert!(!member_started_with(
            Clock::Wall,
            at_check,
            mac_elapsed - 30,
            spawned
        ));

        #[cfg(target_os = "linux")]
        assert_eq!(GROUP_CLOCK, Clock::Boot);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(GROUP_CLOCK, Clock::Wall);
        // The clock since the boot moves on with the wall clock.
        let before = Now::read();
        assert!(before.boot > 0, "{before:?}");
        assert!(Now::read().boot >= before.boot);
    }

    /// Makes `flock` fail with `errno` in this thread until dropped.
    struct LockFails;
    impl LockFails {
        fn with(errno: i32) -> LockFails {
            LOCK_FAILS_WITH.set(Some(errno));
            LockFails
        }
    }
    impl Drop for LockFails {
        fn drop(&mut self) {
            LOCK_FAILS_WITH.set(None);
        }
    }

    /// Where the state folder's file system keeps no locks (some network and FUSE mounts), startup
    /// records its commands, drops the ended records, and disable ends the groups, without the lock.
    /// Any other error of the lock is still an error.
    #[test]
    fn records_are_kept_dropped_and_ended_without_a_lock_where_the_file_system_keeps_none() {
        let (_dir, paths, cfg) = ctx_parts();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-running-test-unused");
        let bin = std::path::PathBuf::from("/bin/true");
        let ctx = plugin_ctx(&tmux, &cfg, &paths, &bin);
        let boot = boot_id().unwrap();

        let mut no_locks = vec![libc::ENOLCK, libc::EOPNOTSUPP, libc::ENOSYS];
        if libc::ENOTSUP != libc::EOPNOTSUPP {
            no_locks.push(libc::ENOTSUP);
        }
        for errno in no_locks {
            let _fails = LockFails::with(errno);
            let mut leader = start_group("exec sleep 30");
            let pid = leader.id();
            let _guard = Guard(pid as libc::pid_t);
            record(&ctx, "nolock", pid);
            let path = pid_file(&ctx, "nolock");
            let written = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                written.starts_with(&format!("{pid}\t")),
                "errno {errno}: no record in {written:?}"
            );

            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{pid}\t-\tan-earlier-boot\t0").unwrap();
            drop(file);
            forget_gone(&ctx, "nolock");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                written,
                "errno {errno}: the ended record was not dropped"
            );

            assert_eq!(stop(&ctx, "nolock"), 1, "errno {errno}");
            leader.wait().unwrap();
            assert!(!path.exists(), "errno {errno}: the record was kept");
        }

        let _fails = LockFails::with(libc::EIO);
        let mut leader = start_group("exec sleep 30");
        let pid = leader.id();
        let _guard = Guard(pid as libc::pid_t);
        record(&ctx, "io", pid);
        let path = pid_file(&ctx, "io");
        assert_eq!(std::fs::read_to_string(&path).unwrap_or_default(), "");
        let line = format!(
            "{pid}\t{}\t{boot}\t{}\n",
            started_at(pid).unwrap(),
            spawned_now()
        );
        std::fs::write(&path, &line).unwrap();
        assert!(keep_records(&path, |_| false).is_err());
        assert_eq!(stop(&ctx, "io"), 0);
        assert!(started_at(pid).is_some(), "the group was ended");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), line);
        // SAFETY: a signal to the one child this test started.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        leader.wait().unwrap();
    }

    #[test]
    fn elapsed_times_read_as_ps_prints_them() {
        assert_eq!(parse_elapsed("00:07"), Some(7));
        assert_eq!(parse_elapsed("  05:03"), Some(303));
        assert_eq!(parse_elapsed("01:02:03"), Some(3723));
        assert_eq!(parse_elapsed("2-01:02:03"), Some(2 * 86400 + 3723));
        assert_eq!(parse_elapsed(""), None);
        assert_eq!(parse_elapsed("7"), None);
        assert_eq!(parse_elapsed("a:b"), None);
        assert!(started_with(100, 100) && started_with(98, 100) && started_with(110, 100));
        assert!(!started_with(97, 100) && !started_with(111, 100));
    }

    /// Every `ps` taw runs (the start time on macOS, the group members everywhere) has its locale and
    /// time zone pinned, whatever they are where taw runs, so this fails on a host that is already in
    /// UTC and the C locale too when `ps()` stops pinning them.
    #[test]
    fn ps_runs_with_its_locale_and_time_zone_pinned() {
        let command = ps();
        let envs: Vec<_> = command.get_envs().collect();
        for (name, value) in [("LC_ALL", "C"), ("TZ", "UTC0")] {
            assert!(
                envs.contains(&(
                    std::ffi::OsStr::new(name),
                    Some(std::ffi::OsStr::new(value))
                )),
                "{name} is not {value} in {envs:?}"
            );
        }
        assert_eq!(command.get_program(), "ps");
    }

    /// A start time recorded under one locale and time zone is read the same under another, so a
    /// `disable` run from a shell with other settings than the tmux server's still knows the leader.
    /// (Linux reads the start time from `/proc`, with no `ps`.)
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_start_time_reads_the_same_under_any_locale_and_time_zone() {
        let mut leader = start_group("exec sleep 30");
        let pid = leader.id();
        let _guard = Guard(pid as libc::pid_t);
        let under = |tz: &str, locale: &str| {
            let mut command = std::process::Command::new("ps");
            command
                .env("TZ", tz)
                .env("LC_ALL", locale)
                .env("LANG", locale);
            started_at_by(pid, pinned(command)).unwrap()
        };
        let tokyo = under("Asia/Tokyo", "fr_FR.UTF-8");
        let new_york = under("America/New_York", "de_DE.UTF-8");
        assert_eq!(tokyo, new_york);
        assert_eq!(tokyo, started_at(pid).unwrap());
        // SAFETY: a signal to the one child this test started.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        leader.wait().unwrap();
    }
}
