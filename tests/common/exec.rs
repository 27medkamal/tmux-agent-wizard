//! Making a test's stand-in program (a fake `tmux`, `ssh`, agent and so on) executable so that
//! Linux runs it at once. A file of its own, so a test binary that has no use for the rest of
//! `common` can include just this (`#[path = "common/exec.rs"] mod exec;`).
//!
//! Linux refuses to run a file that any process holds open for writing ("Text file busy"). A
//! child that another test thread forks while this test still has the stand-in open for writing
//! keeps a copy of that descriptor until it execs, so a plain chmod after the write leaves a
//! window in which running the stand-in fails. [`make_executable`] has a child process copy the
//! file to a new sibling and renames the copy over it, so the file at the path is a new inode
//! that no thread of the test binary ever held open for writing. Do not put a plain chmod back
//! on Linux; the other systems have no such rule and take a plain chmod.
//! The library's own tests have the same helper in `src/test_exec.rs`.

// Each test binary uses a subset of it.
#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Makes the file just written at `path` executable (0o755), as a new inode that no thread of
/// this process ever held open for writing (see the module comment). Panics if it cannot.
pub fn make_executable(path: impl AsRef<Path>) {
    make_executable_with_mode(path, 0o755);
}

/// [`make_executable`] with the given mode, for a site that wants one other than 0o755.
pub fn make_executable_with_mode(path: impl AsRef<Path>, mode: u32) {
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
