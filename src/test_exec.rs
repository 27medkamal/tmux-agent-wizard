//! Making a test's stand-in program (a fake `tmux`, `ssh`, agent and so on) executable so that
//! Linux runs it at once.
//!
//! Linux refuses to run a file that any process holds open for writing ("Text file busy"). A
//! child that another test thread forks while this test still has the stand-in open for writing
//! keeps a copy of that descriptor until it execs, so a plain chmod after the write leaves a
//! window in which running the stand-in fails. [`make_executable`] has a child process copy the
//! file to a new sibling and renames the copy over it, so the file at the path is a new inode
//! that no thread of the test binary ever held open for writing. Do not put a plain chmod back
//! on Linux; the other systems have no such rule and take a plain chmod.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Makes the file just written at `path` executable (0o755), as a new inode that no thread of
/// this process ever held open for writing (see the module comment). Panics if it cannot.
pub(crate) fn make_executable(path: impl AsRef<Path>) {
    make_executable_with_mode(path, 0o755);
}

/// [`make_executable`] with the given mode, for a site that wants one other than 0o755.
pub(crate) fn make_executable_with_mode(path: impl AsRef<Path>, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = path.as_ref();
    // Only Linux refuses to run a file that some process holds open for writing (see the module
    // comment). Elsewhere a chmod is all it takes, and on macOS cp can fail outright on a binary
    // the test just copied, such as a bash under another name, while the system changes that
    // binary's attributes (`Attribute not found`).
    if !cfg!(target_os = "linux") {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .unwrap_or_else(|e| panic!("make_executable: chmod {}: {e}", path.display()));
        return;
    }
    let name = path
        .file_name()
        .unwrap_or_else(|| panic!("make_executable: {} names no file", path.display()))
        .to_string_lossy();
    let copy = path.with_file_name(format!(
        ".{name}.exec-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // The copy is written by `cp`, a child process of its own: this process never opens it.
    let out = std::process::Command::new("/bin/cp")
        .arg("--")
        .arg(path)
        .arg(&copy)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("make_executable: running cp for {}: {e}", path.display()));
    // A copy cp left part-written, or one chmod refused, goes before the panic.
    if !out.status.success() {
        let _ = std::fs::remove_file(&copy);
        panic!(
            "make_executable: cp {} {} failed ({}): {}",
            path.display(),
            copy.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Neither chmod nor rename opens the file.
    if let Err(e) = std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(mode)) {
        let _ = std::fs::remove_file(&copy);
        panic!("make_executable: chmod {}: {e}", copy.display());
    }
    std::fs::rename(&copy, path).unwrap_or_else(|e| {
        let _ = std::fs::remove_file(&copy);
        panic!(
            "make_executable: renaming {} over {}: {e}",
            copy.display(),
            path.display()
        )
    });
}

#[cfg(test)]
mod tests {
    use super::make_executable;

    /// The race itself, run where it can happen: macOS runs a file that is open for writing, so
    /// there the test could not fail, and its spawners would only load the tests beside it.
    #[cfg(target_os = "linux")]
    mod race {
        use super::make_executable;
        use std::path::Path;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Rounds of write, make executable and run, while [`SPAWNERS`] threads start `true`.
        const ROUNDS: usize = 300;
        const SPAWNERS: usize = 3;

        /// Sets `stop` when dropped, so the spawners end even when a round panics, and do not
        /// load every test that runs after this one.
        struct StopOnDrop(Arc<AtomicBool>);

        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        /// Runs a stand-in the moment it is executable, many times over, while other threads of
        /// this binary keep starting processes, and fails on any spawn error. With a plain chmod
        /// in this process in place of [`make_executable`], a child forked by a spawner while the
        /// stand-in was open for writing holds a writer on it and the run fails with "Text file
        /// busy" (os error 26).
        #[test]
        fn a_stand_in_runs_at_once_while_other_threads_start_processes() {
            let dir = tempfile::tempdir().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let _stop_on_drop = StopOnDrop(Arc::clone(&stop));
            let spawners: Vec<_> = (0..SPAWNERS)
                .map(|_| {
                    let stop = Arc::clone(&stop);
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            let _ = std::process::Command::new("true").status();
                        }
                    })
                })
                .collect();
            let mut failures = Vec::new();
            for round in 0..ROUNDS {
                let stand_in = dir.path().join(format!("stand-in-{round}"));
                write_and_run(&stand_in, &mut failures, round);
            }
            stop.store(true, Ordering::Relaxed);
            for spawner in spawners {
                spawner.join().unwrap();
            }
            assert!(
                failures.is_empty(),
                "{} of {ROUNDS} runs failed, the first: {}",
                failures.len(),
                failures[0]
            );
        }

        fn write_and_run(stand_in: &Path, failures: &mut Vec<String>, round: usize) {
            std::fs::write(stand_in, "#!/bin/sh\nexit 0\n").unwrap();
            make_executable(stand_in);
            match std::process::Command::new(stand_in).status() {
                Ok(status) if status.success() => {}
                Ok(status) => failures.push(format!("round {round}: {status}")),
                Err(e) => failures.push(format!("round {round}: {e}")),
            }
        }
    }

    #[test]
    fn the_file_keeps_its_bytes_takes_the_mode_and_no_copy_is_left() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        std::fs::write(&file, "#!/bin/sh\necho hi\n").unwrap();
        make_executable(&file);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tool"], "{names:?}");
        let out = std::process::Command::new(&file).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    }
}
