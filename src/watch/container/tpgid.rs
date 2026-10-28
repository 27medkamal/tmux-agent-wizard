//! The foreground process group of a process's terminal, asked of the system instead of `ps`.
//!
//! The watcher asks it once a second for every pane whose agent is in front and printing, and a
//! `ps` is a process started (and, on macOS, a setuid one) each time. The kernel has the answer in
//! its process table: on macOS `proc_pidinfo(PROC_PIDTBSDINFO)` returns the `proc_bsdinfo` whose
//! `e_tpgid` is the `tpgid` that `ps` prints, and on Linux it is field 8 of `/proc/<pid>/stat`.
//! Other systems have no such call, and the caller reads `ps` there, as it does whenever this read
//! fails.

/// What the system said of the terminal in front of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tpgid {
    /// The foreground group of its terminal; `None` for a process with no terminal.
    Group(Option<u32>),
    /// There is no such process.
    Gone,
    /// The system could not be asked, or did not answer: `ps` is to be.
    Failed,
}

/// The foreground group of the terminal of the process `pid`.
pub fn of(pid: u32) -> Tpgid {
    read(pid)
}

#[cfg(target_os = "macos")]
fn read(pid: u32) -> Tpgid {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        // No pid is that large: nothing runs as it.
        return Tpgid::Gone;
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
    if wrote == size {
        return group(i64::from(info.e_tpgid));
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Tpgid::Gone;
    }
    Tpgid::Failed
}

#[cfg(target_os = "linux")]
fn read(pid: u32) -> Tpgid {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat_tpgid(&stat).map_or(Tpgid::Failed, group),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Tpgid::Gone,
        Err(_) => Tpgid::Failed,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read(_pid: u32) -> Tpgid {
    Tpgid::Failed
}

/// What a terminal group of `tpgid` means: none for a process with no terminal (0 or -1).
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn group(tpgid: i64) -> Tpgid {
    Tpgid::Group(u32::try_from(tpgid).ok().filter(|group| *group > 0))
}

/// The `tpgid` of a `/proc/<pid>/stat` line: field 8. The second field is the command in
/// parentheses, and it can hold spaces and parentheses itself, so the fields are counted from
/// the last `)`: state, ppid, pgrp, session, tty_nr, tpgid.
#[cfg(any(target_os = "linux", test))]
fn stat_tpgid(stat: &str) -> Option<i64> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(5)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_eight_of_proc_stat_is_found_after_a_command_with_spaces_and_parentheses() {
        assert_eq!(
            stat_tpgid("812 (bash) S 811 812 812 34816 812 4194560 0"),
            Some(812)
        );
        assert_eq!(
            stat_tpgid("12 (a) b (c) R 1 12 12 0 -1 4194560 0"),
            Some(-1)
        );
        assert_eq!(stat_tpgid("12 (short) S 1"), None);
        assert_eq!(stat_tpgid("no command at all"), None);
        assert_eq!(group(-1), Tpgid::Group(None));
        assert_eq!(group(0), Tpgid::Group(None));
        assert_eq!(group(812), Tpgid::Group(Some(812)));
    }

    #[test]
    fn a_pid_nothing_runs_as_is_gone() {
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert_eq!(of(u32::MAX - 1), Tpgid::Gone);
        }
    }
}
