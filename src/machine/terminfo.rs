//! Whether this machine knows a terminal type, looked up the way ncurses looks it up, without running a
//! program.
//!
//! A far `tmux attach-session` run for a terminal whose type this machine's terminfo does not have prints
//! `missing or unsuitable terminal: xterm-ghostty` and ends 1. `taw machine attach` asks here first and runs
//! the client with [`FALLBACK`] when the answer is no.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The terminal type a client is run with when its own is not known here: every terminfo database has it.
pub const FALLBACK: &str = "xterm-256color";

/// The places a terminfo database is kept when no variable says otherwise, in the order ncurses reads them.
pub const SYSTEM_DIRS: [&str; 5] = [
    "/etc/terminfo",
    "/lib/terminfo",
    "/usr/share/terminfo",
    "/usr/lib/terminfo",
    "/usr/share/lib/terminfo",
];

/// The variables the lookup reads, taken apart from the process so that a test hands in its own.
#[derive(Clone, Debug, Default)]
pub struct Env {
    /// `$TERMINFO`: one folder, read first.
    pub terminfo: Option<PathBuf>,
    /// The home folder, whose `.terminfo` is read next.
    pub home: Option<PathBuf>,
    /// `$TERMINFO_DIRS`: folders separated by `:`, where an empty entry stands for the default folders.
    pub terminfo_dirs: Option<String>,
}

impl Env {
    /// The variables of this process.
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        Env {
            terminfo: var("TERMINFO").map(PathBuf::from),
            home: var("HOME").map(PathBuf::from),
            terminfo_dirs: var("TERMINFO_DIRS").map(|dirs| dirs.to_string_lossy().into_owned()),
        }
    }
}

/// The folders to look in, in order: `$TERMINFO`, `~/.terminfo`, each entry of `$TERMINFO_DIRS` (an empty
/// entry is `system`), then `system`.
fn folders(env: &Env, system: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(env.terminfo.iter().cloned());
    out.extend(env.home.iter().map(|home| home.join(".terminfo")));
    if let Some(dirs) = &env.terminfo_dirs {
        for entry in dirs.split(':') {
            if entry.is_empty() {
                out.extend(system.iter().cloned());
            } else {
                out.push(PathBuf::from(entry));
            }
        }
    }
    out.extend(system.iter().cloned());
    out
}

/// Whether `<dir>/<first char>/<name>` or `<dir>/<hex of the first byte>/<name>` (macOS) is a file.
fn has_entry(dir: &Path, name: &str) -> bool {
    let Some(first) = name.bytes().next() else {
        return false;
    };
    let letter = dir.join(char::from(first).to_string()).join(name);
    let hex = dir.join(format!("{first:02x}")).join(name);
    letter.is_file() || hex.is_file()
}

/// Whether the terminal type `term` has an entry in a folder of `env` or of `system`. A name that is empty
/// or holds a `/` names no entry (ncurses refuses it too).
pub fn knows(term: &str, env: &Env, system: &[PathBuf]) -> bool {
    if term.is_empty() || term.contains('/') {
        return false;
    }
    folders(env, system).iter().any(|dir| has_entry(dir, term))
}

/// What a client run here is given as `$TERM` in place of its own: [`FALLBACK`] when `term` is unset, empty
/// or not known here, and `None` (keep it) when it is known.
pub fn replacement(term: Option<&OsStr>, env: &Env, system: &[PathBuf]) -> Option<&'static str> {
    let known = term
        .and_then(OsStr::to_str)
        .is_some_and(|term| knows(term, env, system));
    (!known).then_some(FALLBACK)
}

/// [`replacement`] for this process: its `$TERM`, its variables and the default folders.
pub fn replacement_here() -> Option<&'static str> {
    let system: Vec<PathBuf> = SYSTEM_DIRS.iter().map(PathBuf::from).collect();
    replacement(
        std::env::var_os("TERM").as_deref(),
        &Env::from_process(),
        &system,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder with the entry `name` kept as `<sub>/<name>`.
    fn put(dir: &Path, sub: &str, name: &str) {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
        std::fs::write(dir.join(sub).join(name), b"entry").unwrap();
    }

    #[test]
    fn the_letter_layout_is_found() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "x", "xterm-ghostty");
        let env = Env {
            terminfo: Some(dir.path().to_path_buf()),
            ..Env::default()
        };
        assert!(knows("xterm-ghostty", &env, &[]));
        assert!(!knows("xterm-kitty", &env, &[]));
    }

    #[test]
    fn the_hex_layout_of_macos_is_found() {
        let dir = tempfile::tempdir().unwrap();
        // 'x' is 0x78.
        put(dir.path(), "78", "xterm-ghostty");
        let env = Env {
            terminfo: Some(dir.path().to_path_buf()),
            ..Env::default()
        };
        assert!(knows("xterm-ghostty", &env, &[]));
        // A folder is no entry.
        std::fs::create_dir_all(dir.path().join("78").join("wezterm")).unwrap();
        assert!(!knows("wezterm", &env, &[]));
    }

    #[test]
    fn the_home_folder_and_the_system_folders_are_read() {
        let home = tempfile::tempdir().unwrap();
        put(&home.path().join(".terminfo"), "a", "alacritty");
        let system = tempfile::tempdir().unwrap();
        put(system.path(), "w", "wezterm");
        let env = Env {
            home: Some(home.path().to_path_buf()),
            ..Env::default()
        };
        let system = [system.path().to_path_buf()];
        assert!(knows("alacritty", &env, &system));
        assert!(knows("wezterm", &env, &system));
        assert!(!knows("xterm-kitty", &env, &system));
    }

    #[test]
    fn terminfo_dirs_is_read_and_an_empty_entry_means_the_default_folders() {
        let listed = tempfile::tempdir().unwrap();
        put(listed.path(), "k", "kitty-x");
        let system = tempfile::tempdir().unwrap();
        put(system.path(), "s", "screen");
        let system = [system.path().to_path_buf()];
        let env = |dirs: String| Env {
            terminfo_dirs: Some(dirs),
            ..Env::default()
        };

        let plain = env(format!("/nowhere:{}", listed.path().display()));
        assert!(knows("kitty-x", &plain, &[]));
        // The default folders are read after the variable's own either way.
        assert!(knows("screen", &plain, &system));

        // With an empty entry the defaults come where the entry is: a name only the defaults have is found
        // with no listed folder at all.
        let empty = env(":".to_string());
        assert!(knows("screen", &empty, &system));
        assert!(!knows("kitty-x", &empty, &system));
    }

    #[test]
    fn the_first_variable_is_read_before_the_others() {
        // Only the order matters for ncurses when two folders have the name; here both answer yes.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        put(b.path(), "t", "tmux-256color");
        let env = Env {
            terminfo: Some(a.path().to_path_buf()),
            terminfo_dirs: Some(b.path().display().to_string()),
            ..Env::default()
        };
        assert!(knows("tmux-256color", &env, &[]));
    }

    #[test]
    fn a_name_that_is_empty_or_holds_a_slash_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "x", "xterm");
        let env = Env {
            terminfo: Some(dir.path().to_path_buf()),
            ..Env::default()
        };
        assert!(knows("xterm", &env, &[]));
        assert!(!knows("", &env, &[]));
        assert!(!knows("x/xterm", &env, &[]));
        assert!(!knows("../x/xterm", &env, &[]));
    }

    #[test]
    fn replacement_is_the_fallback_for_an_unset_empty_or_unknown_term_only() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "x", "xterm-ghostty");
        let env = Env {
            terminfo: Some(dir.path().to_path_buf()),
            ..Env::default()
        };
        let os = |text: &str| std::ffi::OsString::from(text);
        assert_eq!(replacement(None, &env, &[]), Some(FALLBACK));
        assert_eq!(replacement(Some(&os("")), &env, &[]), Some(FALLBACK));
        assert_eq!(
            replacement(Some(&os("xterm-kitty")), &env, &[]),
            Some(FALLBACK)
        );
        assert_eq!(replacement(Some(&os("xterm-ghostty")), &env, &[]), None);
    }
}
