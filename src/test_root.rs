//! For the tests that rest on what root is exempt from: the modes of files and folders, and
//! who may open a terminal. Run as root (a `cargo test` in a container), each returns early
//! through [`skip_as_root`], with a line saying why, so the run is green and says what it did
//! not check.

/// Whether this process's effective user is root, in which case it prints `skipped as root:
/// <why>` for the test to return on.
pub(crate) fn skip_as_root(why: &str) -> bool {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return false;
    }
    eprintln!("skipped as root: {why}");
    true
}
