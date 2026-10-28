//! Directory candidates for the "new session" picker: path completion under a typed
//! prefix, `zoxide`'s ranked history, and configured project directories. Read from the
//! real filesystem.
//!
//! The fuzzy matcher is injected (`rank`'s `score`) so this module stays free of
//! `nucleo-matcher`; the popup passes nucleo's own scorer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::session::naming::{expand_tilde, tilde};

/// Where a [`Candidate`] came from — completion always sorts first; the other two are
/// scored and interleaved by [`rank`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Completion,
    Zoxide,
    ProjectDir,
}

/// One directory the "new session" picker could open, already in tilde form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Tilde form, no trailing `/`.
    pub path: String,
    pub source: Source,
    /// `true` when Tab should append `/` to keep completing into this directory, rather
    /// than treat it as the final choice.
    pub is_dir_prefix: bool,
}

/// `typed` (`~/...`, `~`, or an absolute path) resolved against `home` to a real
/// filesystem path, without requiring it to exist.
fn to_absolute(typed: &str, home: &Path) -> PathBuf {
    expand_tilde(typed, home)
}

/// Directory completions for `typed`: if it already names a real directory, its
/// children; otherwise the children of its parent whose names start with `typed`'s last
/// segment (case-insensitive). Files are never candidates, and a `typed` whose parent
/// does not exist yields none. Every result is a completion candidate with
/// `is_dir_prefix: true`, sorted by name.
///
/// As a shell completes, a folder whose name starts with a dot (a repository's `.git`,
/// say) is left out unless the part of `typed` after its last slash starts with a dot too.
pub fn complete_path(typed: &str, home: &Path) -> Vec<Candidate> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Vec::new();
    }
    let last_typed = typed.rsplit('/').next().unwrap_or("");
    let dot_typed = last_typed.starts_with('.');
    let absolute = to_absolute(typed, home);
    let (dir, prefix) = if absolute.is_dir() {
        // `dir/.` names `dir`, and what is wanted there are its dot folders.
        let prefix = (last_typed == ".").then(|| ".".to_string());
        (absolute.components().collect::<PathBuf>(), prefix)
    } else {
        match absolute.parent() {
            Some(parent) => {
                let last = absolute
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (parent.to_path_buf(), Some(last))
            }
            None => return Vec::new(),
        }
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix_lower = prefix.as_deref().unwrap_or("").to_lowercase();
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| dot_typed || !name.starts_with('.'))
        .filter(|name| prefix.is_none() || name.to_lowercase().starts_with(&prefix_lower))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| Candidate {
            path: tilde(&dir.join(name), home),
            source: Source::Completion,
            is_dir_prefix: true,
        })
        .collect()
}

/// `zoxide`'s ranked directories, tilde-formed against the real home directory: `runner`
/// runs `zoxide query -l` (paths only, zoxide's own order) and returns its stdout, or
/// `None` when the `zoxide` binary is absent (so the feature is simply off, not an
/// error).
///
/// The parser is defensive regardless: [`strip_zoxide_score`] drops a leading score if
/// one is there anyway (a user alias that still adds `-s`), and skips a line that is
/// neither a scored nor a bare path.
pub fn zoxide_candidates(runner: &dyn Fn(&[&str]) -> Option<String>) -> Vec<String> {
    let Some(stdout) = runner(&["query", "-l"]) else {
        return Vec::new();
    };
    let home = dirs::home_dir();
    stdout
        .lines()
        .filter_map(strip_zoxide_score)
        .map(|path| match &home {
            Some(home) => tilde(Path::new(path), home),
            None => path.to_string(),
        })
        .collect()
}

/// `line` with a leading `zoxide query -ls`-style score dropped, when there is one: its
/// first whitespace-separated token parses as a number and the remainder starts with `/`
/// or `~`. Returns the (trimmed) whole line when it is already a bare absolute or tilde
/// path, and `None` when it is neither — a blank line, or garbage on `zoxide`'s stdout.
fn strip_zoxide_score(line: &str) -> Option<&str> {
    let line = line.trim();
    if let Some((first, rest)) = line.split_once(char::is_whitespace) {
        let rest = rest.trim_start();
        if first.parse::<f64>().is_ok() && (rest.starts_with('/') || rest.starts_with('~')) {
            return Some(rest);
        }
    }
    (line.starts_with('/') || line.starts_with('~')).then_some(line)
}

/// The children of each configured project directory, tilde-formed. A directory that
/// cannot be read (missing, not a directory, permissions) contributes nothing.
pub fn project_dir_candidates(dirs: &[PathBuf], home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        out.extend(children.iter().map(|path| tilde(path, home)));
    }
    out
}

/// Merges completion candidates with scored `Zoxide`/`ProjectDir` ones: completion stays
/// first in the order given; the rest are kept only where `score(typed, &candidate.path)`
/// returns `Some`, sorted by score descending, and appended after — deduplicated by path
/// throughout (a source candidate that repeats a completion path, or another source's
/// path, is dropped).
pub fn rank(
    candidates: Vec<Candidate>,
    typed: &str,
    score: &dyn Fn(&str, &str) -> Option<i64>,
) -> Vec<Candidate> {
    let (completion, sources): (Vec<Candidate>, Vec<Candidate>) = candidates
        .into_iter()
        .partition(|c| c.source == Source::Completion);

    let mut scored: Vec<(i64, Candidate)> = sources
        .into_iter()
        .filter_map(|c| score(typed, &c.path).map(|s| (s, c)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));

    let mut seen: HashSet<String> = completion.iter().map(|c| c.path.clone()).collect();
    let mut out = completion;
    for (_, candidate) in scored {
        if seen.insert(candidate.path.clone()) {
            out.push(candidate);
        }
    }
    out
}

/// Best-effort `zoxide add <path>`, so opening or creating a session here also teaches
/// `zoxide` about it. Errors (including a missing binary) are ignored: this is a
/// convenience, never a dependency.
///
/// All three streams are `/dev/null`. The project picker calls this from inside the
/// popup's raw mode and alternate screen, where an inherited stdout or stderr — a
/// `zoxide` warning, say — would be drawn straight over the frame, and an inherited stdin
/// would let the child eat the popup's keystrokes.
pub fn zoxide_add(path: &Path) {
    let _ = std::process::Command::new("zoxide")
        .arg("add")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A temp `home` with `code/github/{docs,web}`, `code/shop/site`,
    /// `oss/api` and `work/api` — the tree every completion test walks.
    fn home_with_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for dir in [
            "code/github/docs",
            "code/github/web",
            "code/shop/site",
            "oss/api",
            "work/api",
        ] {
            fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        tmp
    }

    fn paths(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.path.as_str()).collect()
    }

    /// A temp `home` with `code/docs/{.git,src}` and `code/.cache`, a repository the way
    /// git makes one.
    fn home_with_a_repository() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["code/docs/.git/objects", "code/docs/src", "code/.cache"] {
            fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        tmp
    }

    /// As a shell completes: a folder whose name starts with a dot is offered only when the
    /// part typed after the last slash starts with a dot too.
    #[test]
    fn completion_leaves_out_dot_folders_unless_a_dot_is_typed() {
        let home = home_with_a_repository();
        let at = |typed: &str| paths_of(&complete_path(typed, home.path()));
        assert_eq!(at("~/code/docs/"), vec!["~/code/docs/src"]);
        assert_eq!(at("~/code/docs"), vec!["~/code/docs/src"]);
        assert_eq!(at("~/code/"), vec!["~/code/docs"]);
        assert_eq!(at("~/code/d"), vec!["~/code/docs"]);
        assert_eq!(at("~/code/docs/."), vec!["~/code/docs/.git"]);
        assert_eq!(at("~/code/docs/.g"), vec!["~/code/docs/.git"]);
        assert_eq!(at("~/code/."), vec!["~/code/.cache"]);
    }

    fn paths_of(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.path.clone()).collect()
    }

    #[test]
    fn completion_lists_children_and_prefix_matches() {
        let home = home_with_tree();

        let wb = complete_path("~/cod", home.path());
        assert_eq!(paths(&wb), vec!["~/code"]);
        assert!(wb[0].is_dir_prefix);
        assert_eq!(wb[0].source, Source::Completion);

        let code_slash = complete_path("~/code/", home.path());
        assert_eq!(paths(&code_slash), vec!["~/code/github", "~/code/shop"]);

        let code_g = complete_path("~/code/g", home.path());
        assert_eq!(paths(&code_g), vec!["~/code/github"]);
    }

    #[test]
    fn completion_ignores_files_and_missing_parents() {
        let home = home_with_tree();
        fs::write(home.path().join("code/github/README.md"), b"hi").unwrap();

        let github = complete_path("~/code/github/", home.path());
        assert_eq!(
            paths(&github),
            vec!["~/code/github/docs", "~/code/github/web"]
        );

        let missing = complete_path("~/nope/x", home.path());
        assert!(missing.is_empty());
    }

    #[test]
    fn zoxide_is_off_when_the_binary_is_missing() {
        assert!(zoxide_candidates(&|_| None).is_empty());
    }

    #[test]
    fn zoxide_output_is_split_by_line_and_tilded() {
        let home = dirs::home_dir().expect("test environment must have a home directory");
        let a = home.join("zoxide-candidate-a");
        let b = home.join("zoxide-candidate-b");
        let stdout = format!("{}\n{}\n", a.display(), b.display());

        let out = zoxide_candidates(&|args| {
            assert_eq!(args, ["query", "-l"]);
            Some(stdout.clone())
        });

        assert_eq!(out, vec!["~/zoxide-candidate-a", "~/zoxide-candidate-b"]);
    }

    /// A user alias that still adds `-s` (or hasn't picked up the `-l` change some other
    /// way) leaves a score on the front of each line: `zoxide_candidates` must drop it
    /// rather than pass `"2580.0 /Users/alice/a"` on to `open_or_create` as a path.
    #[test]
    fn zoxide_strips_a_leading_score_when_present() {
        let home = dirs::home_dir().expect("test environment must have a home directory");
        let a = home.join("zoxide-candidate-a");
        let b = home.join("zoxide-candidate-b");
        // Real `-ls` output right-aligns the score column, so the second line's score
        // carries a leading space of its own before the line is trimmed.
        let stdout = format!("2580.0 {}\n 680.0 {}\n", a.display(), b.display());

        let out = zoxide_candidates(&|_| Some(stdout.clone()));

        assert_eq!(out, vec!["~/zoxide-candidate-a", "~/zoxide-candidate-b"]);
    }

    #[test]
    fn zoxide_skips_a_line_that_is_not_a_path() {
        let out = zoxide_candidates(&|_| Some("not a path\n".to_string()));
        assert!(out.is_empty());
    }

    #[test]
    fn strip_zoxide_score_drops_a_leading_numeric_score_only_before_a_path() {
        assert_eq!(
            strip_zoxide_score("2580.0 /Users/alice/a"),
            Some("/Users/alice/a")
        );
        assert_eq!(
            strip_zoxide_score(" 680.0 /Users/alice/b"),
            Some("/Users/alice/b")
        );
        assert_eq!(strip_zoxide_score("/Users/alice/a"), Some("/Users/alice/a"));
        assert_eq!(strip_zoxide_score("~/a"), Some("~/a"));
        assert_eq!(
            strip_zoxide_score("~/a b"),
            Some("~/a b"),
            "a tilde path is not mistaken for a scored line just because it has a space in it"
        );
        assert_eq!(strip_zoxide_score("not a path"), None);
        assert_eq!(strip_zoxide_score(""), None);
    }

    #[test]
    fn project_dirs_list_children() {
        let home = home_with_tree();
        let dirs = vec![
            home.path().join("code/github"),
            home.path().join("code/shop"),
        ];

        let out = project_dir_candidates(&dirs, home.path());

        assert_eq!(
            out,
            vec![
                "~/code/github/docs".to_string(),
                "~/code/github/web".to_string(),
                "~/code/shop/site".to_string(),
            ]
        );
    }

    #[test]
    fn rank_keeps_completion_first_then_scored_sources_deduped() {
        let candidates = vec![
            Candidate {
                path: "~/code/github".to_string(),
                source: Source::Completion,
                is_dir_prefix: true,
            },
            Candidate {
                path: "~/code/github/docs".to_string(),
                source: Source::Zoxide,
                is_dir_prefix: false,
            },
            Candidate {
                path: "~/oss/api".to_string(),
                source: Source::ProjectDir,
                is_dir_prefix: false,
            },
            // A duplicate of the completion entry from a different source: must not
            // appear twice.
            Candidate {
                path: "~/code/github".to_string(),
                source: Source::Zoxide,
                is_dir_prefix: false,
            },
            // Scores to None: dropped entirely.
            Candidate {
                path: "~/unrelated".to_string(),
                source: Source::Zoxide,
                is_dir_prefix: false,
            },
        ];
        let score = |_typed: &str, path: &str| -> Option<i64> {
            match path {
                "~/code/github/docs" => Some(5),
                "~/oss/api" => Some(50),
                "~/code/github" => Some(1),
                _ => None,
            }
        };

        let out = rank(candidates, "d", &score);

        assert_eq!(
            paths(&out),
            vec!["~/code/github", "~/oss/api", "~/code/github/docs"]
        );
    }
}
