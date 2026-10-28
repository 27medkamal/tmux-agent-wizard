// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! The exact arguments of a process, as it was started with them.
//!
//! `ps -o args=` joins them with single spaces. Quotes and boundaries are gone, so a value
//! with a space in it is two words, an empty argument leaves no trace and a newline is an
//! escape. The list itself is asked of the system instead, the way herdr's macOS platform
//! reads a process's argv: the kernel's `KERN_PROCARGS2` on macOS, `/proc/<pid>/cmdline` on
//! Linux. Other systems have neither, and the caller reads `ps` there.
//!
//! What the macOS call returns holds the environment after the arguments, and an environment
//! can carry credentials. The parser stops after the `argc` arguments and drops the rest of
//! the buffer, so the environment is not read, kept or logged. One shape gets past that: the
//! NULs that pad the executable path are skipped as a run, an empty `argv[0]` is one more
//! NUL in it, and so a process started with an empty `argv[0]` gives a list that starts at
//! `argv[1]` and ends with the first string of its environment. Such a list does not start
//! with the runtime, so `args_of` drops it, and nothing of it is kept or logged.

/// The arguments of the process `pid`, its own name first, or `None` when they cannot be
/// read: no such process, one of another user, one in the middle of an `exec`, or a system
/// with no way to ask.
pub fn of(pid: u32) -> Option<Vec<String>> {
    read(pid)
}

#[cfg(target_os = "macos")]
fn read(pid: u32) -> Option<Vec<String>> {
    procargs2_argv(&kern_procargs2(pid)?)
}

#[cfg(target_os = "linux")]
fn read(pid: u32) -> Option<Vec<String>> {
    cmdline_argv(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read(_pid: u32) -> Option<Vec<String>> {
    None
}

/// Raw `sysctl(KERN_PROCARGS2)`: the arguments and the environment of the process in one
/// buffer.
#[cfg(target_os = "macos")]
fn kern_procargs2(pid: u32) -> Option<Vec<u8>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        libc::c_int::try_from(pid).ok()?,
    ];
    let mut size: libc::size_t = 0;
    // SAFETY: `mib` is three ints and the call is told so. With no buffer to fill it only
    // stores in `size` how many bytes the answer needs.
    let asked = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if asked != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is `size` bytes that can be written to and `size` says so, so the kernel
    // writes no more than that and leaves in `size` how many bytes it wrote.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buf.truncate(size);
    Some(buf)
}

/// The arguments in a `KERN_PROCARGS2` buffer:
/// `[argc: i32] [exec_path\0] [padding \0...] [argv[0]\0] ... [argv[argc-1]\0] [env\0]...`.
#[cfg(any(target_os = "macos", test))]
fn procargs2_argv(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|argc| *argc > 0)?;
    let rest = &buf[4..];
    // Past the executable path and the padding after it, to `argv[0]`.
    let exec_end = rest.iter().position(|&byte| byte == 0)?;
    let padding = rest[exec_end..]
        .iter()
        .take_while(|&&byte| byte == 0)
        .count();
    let mut at = exec_end + padding;
    if at >= rest.len() {
        return None;
    }
    let mut argv = Vec::with_capacity(argc.min(256));
    for _ in 0..argc {
        let arg = rest.get(at..).filter(|arg| !arg.is_empty())?;
        // An argument is what runs up to its terminator, and an empty one is only that.
        let end = arg.iter().position(|&byte| byte == 0).unwrap_or(arg.len());
        argv.push(String::from_utf8_lossy(&arg[..end]).into_owned());
        at += end + 1;
    }
    Some(argv)
}

/// The arguments in `/proc/<pid>/cmdline`: each one followed by a NUL.
#[cfg(any(target_os = "linux", test))]
fn cmdline_argv(bytes: &[u8]) -> Option<Vec<String>> {
    // Nothing for a zombie or a kernel thread.
    if bytes.is_empty() {
        return None;
    }
    let body = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    Some(
        body.split(|&byte| byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    /// A `KERN_PROCARGS2` buffer for a process: what the kernel lays out, with the
    /// environment after the arguments.
    fn procargs2(exec_path: &str, argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i32::try_from(argv.len()).unwrap().to_ne_bytes());
        buf.extend_from_slice(exec_path.as_bytes());
        buf.push(0);
        // The path is padded with NULs before the first argument.
        buf.extend_from_slice(&[0, 0, 0]);
        for arg in argv.iter().chain(env) {
            buf.extend_from_slice(arg.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn the_arguments_come_out_of_a_procargs2_buffer_and_the_environment_does_not() {
        let buf = procargs2(
            "/usr/local/bin/docker",
            &["docker", "run", "--rm", "img", "claude"],
            &["PATH=/usr/bin", "ANTHROPIC_API_KEY=sk-secret", "run"],
        );
        let argv = procargs2_argv(&buf).expect("a buffer with arguments");
        assert_eq!(argv, ["docker", "run", "--rm", "img", "claude"]);
        assert!(!argv.iter().any(|arg| arg.contains("secret")));
    }

    #[test]
    fn empty_arguments_and_odd_ones_are_kept_as_they_are() {
        // What a `ps` line cannot say: an empty argument, a newline, a value with spaces.
        let buf = procargs2(
            "/usr/local/bin/docker",
            &[
                "docker",
                "run",
                "--entrypoint",
                "",
                "-e",
                "MSG=hello world",
                "img",
                "bash",
                "-lc",
                "a;\n  b && c\n",
                "",
            ],
            &["TERM=xterm"],
        );
        assert_eq!(
            procargs2_argv(&buf).unwrap(),
            [
                "docker",
                "run",
                "--entrypoint",
                "",
                "-e",
                "MSG=hello world",
                "img",
                "bash",
                "-lc",
                "a;\n  b && c\n",
                ""
            ]
        );
    }

    #[test]
    fn an_empty_argv0_shifts_the_list_to_end_in_the_first_string_of_the_environment() {
        // The padding after the executable path is a run of NULs, and an empty `argv[0]` is one
        // more in it, so the parser starts at `argv[1]` and, counting `argc` strings, takes the
        // first of the environment as the last. Nothing tells the shape from padding; the
        // list is dropped by `args_of`, whose first word is not the runtime.
        let buf = procargs2(
            "/usr/local/bin/docker",
            &["", "run", "img", "claude"],
            &["TOKEN=secret", "PATH=/usr/bin"],
        );
        assert_eq!(
            procargs2_argv(&buf).unwrap(),
            ["run", "img", "claude", "TOKEN=secret"]
        );
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_read_lossily() {
        let mut buf = procargs2("/bin/x", &["x", "a"], &[]);
        let at = buf.len() - 2;
        buf[at] = 0xff;
        assert_eq!(procargs2_argv(&buf).unwrap(), ["x", "\u{fffd}"]);
    }

    #[test]
    fn a_buffer_that_does_not_hold_what_it_says_is_no_arguments() {
        // Fewer arguments than argc, no arguments, a negative count, and cut short.
        let mut buf = procargs2("/bin/x", &["x", "y"], &[]);
        buf[..4].copy_from_slice(&5i32.to_ne_bytes());
        assert_eq!(procargs2_argv(&buf), None);
        assert_eq!(procargs2_argv(&procargs2("/bin/x", &[], &["A=b"])), None);
        let mut buf = procargs2("/bin/x", &["x"], &[]);
        buf[..4].copy_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(procargs2_argv(&buf), None);
        assert_eq!(procargs2_argv(&[]), None);
        assert_eq!(procargs2_argv(&[1, 0, 0]), None);
        assert_eq!(procargs2_argv(&[1, 0, 0, 0]), None);
        assert_eq!(procargs2_argv(&[1, 0, 0, 0, b'x']), None);
        assert_eq!(procargs2_argv(&[1, 0, 0, 0, b'x', 0, 0]), None);
    }

    #[test]
    fn a_huge_count_reads_what_there_is_and_no_more() {
        let mut buf = procargs2("/bin/x", &["x"], &[]);
        buf[..4].copy_from_slice(&i32::MAX.to_ne_bytes());
        assert_eq!(procargs2_argv(&buf), None);
    }

    #[test]
    fn a_cmdline_is_its_arguments_split_at_the_nuls() {
        assert_eq!(
            cmdline_argv(b"docker\0run\0-e\0A=b c\0\0img\0x\ny\0").unwrap(),
            ["docker", "run", "-e", "A=b c", "", "img", "x\ny"]
        );
        // The last argument is empty: two NULs at the end.
        assert_eq!(cmdline_argv(b"a\0\0").unwrap(), ["a", ""]);
        // A process that rewrote its title leaves no NUL at the end.
        assert_eq!(cmdline_argv(b"a\0b").unwrap(), ["a", "b"]);
        // Bytes that are not UTF-8 are read lossily, as the macOS reader reads them.
        assert_eq!(cmdline_argv(b"a\0\xff\0").unwrap(), ["a", "\u{fffd}"]);
        assert_eq!(cmdline_argv(b""), None);
    }

    #[test]
    fn the_arguments_of_a_running_process_are_read_exactly() {
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "sleep 30; :",
                "zero",
                "a b",
                "multi\nline\t;&|()`'\"$",
                "",
                "q'uote",
            ])
            .env("TAW_ARGV_TEST_SECRET", "hunter2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sh");
        // Until the shell has replaced what `spawn` forked, the arguments are the parent's.
        let deadline = Instant::now() + Duration::from_secs(10);
        let argv = loop {
            match of(child.id()) {
                Some(argv) if argv.first().is_some_and(|program| program == "/bin/sh") => {
                    break argv;
                }
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                other => {
                    let _ = child.kill();
                    panic!("the shell's arguments were never read: {other:?}");
                }
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            argv,
            [
                "/bin/sh",
                "-c",
                "sleep 30; :",
                "zero",
                "a b",
                "multi\nline\t;&|()`'\"$",
                "",
                "q'uote"
            ]
        );
        assert!(!argv.iter().any(|arg| arg.contains("hunter2")));
    }

    #[test]
    fn a_process_nothing_has_is_no_arguments() {
        assert_eq!(of(u32::MAX - 1), None);
        assert_eq!(of(0), None);
    }
}
