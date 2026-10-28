//! Writing a file whole: to a file beside it and then over it, so that a reader never sees half of it.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// How many temporary names one write tries: a name that is already there (a crashed process left it, and
/// its pid came round again) is another writer's file, and the write goes on to the next name.
const TRIES: u32 = 4;

/// Writes `text` to `path` through a temporary file in the same folder, made when the folder is not
/// there. The temporary file is named for the file (`fallback_name` when that is not text), this process
/// and a count, so that two writers in one process or two processes never share one, and it does not
/// stay behind when the write or the rename fails. A file that was already there under a temporary name is
/// not this write's: it is left alone and the next name is tried, four in all. The file is made readable
/// by its owner only (mode 0600): what is written here holds names, titles and paths of other machines
/// and of crashes.
pub(crate) fn write_whole(path: &Path, text: &str, fallback_name: &str) -> io::Result<()> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    write_whole_from(
        path,
        text,
        fallback_name,
        COUNTER.fetch_add(TRIES, Ordering::Relaxed),
    )
}

/// [`write_whole`] with the count of the first temporary name given, so that a test can fix the names.
fn write_whole_from(path: &Path, text: &str, fallback_name: &str, first: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);
    let mut taken = None;
    for count in (0..TRIES).map(|n| first.wrapping_add(n)) {
        let tmp = dir.join(format!(".{name}.{}.{count}.tmp", std::process::id()));
        // Only a file this call made is removed by it: an open that fails made nothing.
        let mut file = match private_file(&tmp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                taken = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let written = file.write_all(text.as_bytes());
        drop(file);
        return written
            .and_then(|()| std::fs::rename(&tmp, path))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            });
    }
    Err(taken.unwrap_or_else(|| io::ErrorKind::AlreadyExists.into()))
}

/// Makes `path` (which must not be there) with mode 0600, open for writing.
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_file_is_replaced_whole_and_the_folder_is_made() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a").join("b").join("file.json");
        write_whole(&path, "one\n", "file").unwrap();
        write_whole(&path, "two\n", "file").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
        assert_eq!(names(path.parent().unwrap()), ["file.json"]);
    }

    #[test]
    fn a_file_is_written_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // The process umask is not changed (it is shared by every test thread). The mode a plain write
        // gets here is read back from a file made beside: when it is already 0600 (a umask of 077) the
        // check below cannot tell the two apart, and passes; under the usual 022 it tells 0644 from 0600.
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        let plain_mode = std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777;
        let path = tmp.path().join("deep").join("file.json");
        write_whole(&path, "one\n", "file").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a plain write here makes {plain_mode:o}");
        // A file that was already there is replaced by one of the same mode, not kept at its old one.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_whole(&path, "two\n", "file").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_failed_rename_leaves_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where the file goes.
        std::fs::create_dir(tmp.path().join("file.json")).unwrap();
        assert!(write_whole(&tmp.path().join("file.json"), "x\n", "file").is_err());
        assert_eq!(names(tmp.path()), ["file.json"]);
    }

    #[test]
    fn a_temporary_name_that_is_taken_is_left_alone_and_the_next_is_tried() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.json");
        let stale = |count: u32| {
            tmp.path()
                .join(format!(".file.json.{}.{count}.tmp", std::process::id()))
        };
        // Another process's leftovers at the names for the counts 7, 8 and 9.
        for count in 7..10 {
            std::fs::write(stale(count), format!("stale {count}")).unwrap();
        }
        write_whole_from(&path, "new\n", "file", 7).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        for count in 7..10 {
            assert_eq!(
                std::fs::read_to_string(stale(count)).unwrap(),
                format!("stale {count}")
            );
        }
        // The fourth name was renamed over the file: nothing else is there.
        assert_eq!(names(tmp.path()).len(), 4);
    }

    #[test]
    fn four_temporary_names_taken_is_an_error_and_every_file_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.json");
        let stale = |count: u32| {
            tmp.path()
                .join(format!(".file.json.{}.{count}.tmp", std::process::id()))
        };
        for count in 7..11 {
            std::fs::write(stale(count), format!("stale {count}")).unwrap();
        }
        let error = write_whole_from(&path, "new\n", "file", 7).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!path.exists());
        for count in 7..11 {
            assert_eq!(
                std::fs::read_to_string(stale(count)).unwrap(),
                format!("stale {count}")
            );
        }
        assert_eq!(names(tmp.path()).len(), 4);
    }
}
