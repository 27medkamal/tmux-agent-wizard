//! tmux session names: three naming modes, tmux's own name-mangling rule, and how to
//! propose a name that dodges an existing collision with the shortest distinguishing
//! path suffix.

use std::path::{Path, PathBuf};

use crate::config::NameMode;

/// The session name tmux will actually use for `raw`: it folds `.` and `:` to `_`
/// (see [`crate::tmux::session_name`] for why), silently.
pub fn tmux_name(raw: &str) -> String {
    raw.chars()
        .map(|c| if c == '.' || c == ':' { '_' } else { c })
        .collect()
}

/// `path` in tilde form when it is under `home` (`~` itself, or `~/rest`); any other
/// path is returned unchanged, and so is every path when no home folder is known (an
/// empty `home`, which every path would otherwise start with).
pub fn tilde(path: &Path, home: &Path) -> String {
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// `input` (`~`, `~/rest`, or anything else) resolved against `home` to a real
/// filesystem path, without requiring it to exist — the inverse of [`tilde`]. Shared
/// by `session::projects` (path completion) and `session::create` (the `path`
/// argument to `open_or_create`) so "expand a leading `~`" has exactly one
/// implementation.
pub fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        home.to_path_buf()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(input)
    }
}

/// `path`'s segments cut to their first two characters, except the last segment, `~`,
/// and empty segments (from a leading or trailing `/`) — the abbreviation
/// `NameMode::ShortPath` wants.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let last_index = parts.len().saturating_sub(1);
    parts
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            if i == last_index || *seg == "~" || seg.is_empty() {
                (*seg).to_string()
            } else {
                seg.chars().take(2).collect::<String>()
            }
        })
        .collect::<Vec<String>>()
        .join("/")
}

/// The session name `tilde_path` maps to under `mode`, before collision handling:
/// `Directory` is its last path segment, `FullPath` is the path as given, `ShortPath` is
/// [`short_path`] of it — each then passed through [`tmux_name`].
pub fn base_name(tilde_path: &str, mode: NameMode) -> String {
    let trimmed = tilde_path.trim_end_matches('/');
    match mode {
        NameMode::FullPath => tmux_name(trimmed),
        NameMode::ShortPath => tmux_name(&short_path(trimmed)),
        NameMode::Directory => {
            let last = trimmed.split('/').rfind(|s| !s.is_empty());
            tmux_name(last.unwrap_or(trimmed))
        }
    }
}

/// One already-running session: its tmux name and its directory, in tilde form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Existing {
    pub name: String,
    pub path: String,
}

/// A proposed session name for a not-yet-running directory, plus a human-readable note
/// when [`session_name_for`] had to fall back to a longer name to dodge a collision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposed {
    pub name: String,
    pub collision: Option<String>,
}

/// The session name to propose for `tilde_path` under `mode`, given the sessions already
/// running. A collision is another entry with the same proposed name and a *different*
/// path (the same path is the same session, not a clash). Only `NameMode::Directory` ever
/// resolves one, by trying progressively longer path suffixes (2, 3, … segments below
/// `~/`) until one is free, falling back to the full path if none is; the other modes
/// report no collision even where one nominally exists, since their name already is (or
/// stands in for) the full path.
pub fn session_name_for(tilde_path: &str, mode: NameMode, existing: &[Existing]) -> Proposed {
    let want = base_name(tilde_path, mode);
    let clash = existing
        .iter()
        .find(|s| s.name == want && s.path != tilde_path);
    let Some(clash) = clash else {
        return Proposed {
            name: want,
            collision: None,
        };
    };
    if mode != NameMode::Directory {
        return Proposed {
            name: want,
            collision: None,
        };
    }
    let why = format!("\"{want}\" is taken by {}", clash.path);
    let stripped = tilde_path.strip_prefix("~/").unwrap_or(tilde_path);
    let parts: Vec<&str> = stripped.split('/').collect();
    let mut n = 2;
    while n <= parts.len() {
        let candidate = tmux_name(&parts[parts.len() - n..].join("/"));
        if !existing
            .iter()
            .any(|s| s.name == candidate && s.path != tilde_path)
        {
            return Proposed {
                name: candidate,
                collision: Some(why),
            };
        }
        n += 1;
    }
    Proposed {
        name: tmux_name(tilde_path),
        collision: Some(why),
    }
}

/// Validates a name the user typed for a new session: refused only where tmux itself
/// would reject or silently rewrite it (empty, or containing `.`/`:`), or where it
/// collides with a session already running (`live_names`, exact tmux names).
pub fn validate_typed(name: &str, live_names: &[String]) -> Result<(), String> {
    if name.is_empty() {
        return Err("a name is needed".to_string());
    }
    if name.contains('.') || name.contains(':') {
        return Err(
            "tmux would rewrite \".\" and \":\" to \"_\", so pick a name without them".to_string(),
        );
    }
    if live_names.iter().any(|existing| existing == name) {
        return Err(format!("duplicate session: {name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NameMode;

    #[test]
    fn tmux_rule_only() {
        assert_eq!(tmux_name("My.App:v2 Foo"), "My_App_v2 Foo");
        assert_eq!(tmux_name("oss/api"), "oss/api");
    }

    #[test]
    fn modes() {
        assert_eq!(base_name("~/oss/api", NameMode::Directory), "api");
        assert_eq!(base_name("~/oss/api", NameMode::FullPath), "~/oss/api");
        assert_eq!(
            base_name("~/code/github/api", NameMode::ShortPath),
            "~/co/gi/api"
        );
        assert_eq!(base_name("~/oss/api.v2", NameMode::Directory), "api_v2");
    }

    #[test]
    fn collision_takes_the_shortest_distinguishing_suffix() {
        let ex = vec![Existing {
            name: "api".into(),
            path: "~/work/api".into(),
        }];
        let p = session_name_for("~/oss/api", NameMode::Directory, &ex);
        assert_eq!(p.name, "oss/api");
        assert!(p.collision.unwrap().contains("~/work/api"));

        let ex2 = vec![
            Existing {
                name: "api".into(),
                path: "~/work/api".into(),
            },
            Existing {
                name: "x/api".into(),
                path: "~/b/x/api".into(),
            },
        ];
        assert_eq!(
            session_name_for("~/a/x/api", NameMode::Directory, &ex2).name,
            "a/x/api"
        );

        assert_eq!(
            session_name_for("~/oss/api", NameMode::FullPath, &ex).name,
            "~/oss/api"
        ); // cannot collide

        assert_eq!(
            session_name_for("~/work/api", NameMode::Directory, &ex).collision,
            None
        ); // same path: not a collision
    }

    #[test]
    fn typed_names_are_refused_only_where_tmux_would() {
        assert!(validate_typed("", &[]).is_err());
        assert!(validate_typed("a.b", &[]).is_err());
        assert!(validate_typed("a:b", &[]).is_err());
        assert!(
            validate_typed("api", &["api".into()])
                .unwrap_err()
                .starts_with("duplicate session")
        );
        assert!(validate_typed("My App/x", &[]).is_ok());
    }

    /// `tilde` is part of the public interface and its "only under home, else unchanged"
    /// contract is easy to get backwards.
    #[test]
    fn tilde_form_only_under_home() {
        let home = std::path::Path::new("/Users/x");
        assert_eq!(tilde(std::path::Path::new("/Users/x/a"), home), "~/a");
        assert_eq!(tilde(home, home), "~");
        assert_eq!(tilde(std::path::Path::new("/etc/foo"), home), "/etc/foo");
        // A folder whose name only starts with home's is not under it.
        assert_eq!(
            tilde(std::path::Path::new("/Users/xy/a"), home),
            "/Users/xy/a"
        );
        // No home folder known: every path stays whole, never `~//etc/foo`.
        let none = std::path::Path::new("");
        assert_eq!(tilde(std::path::Path::new("/etc/foo"), none), "/etc/foo");
    }

    #[test]
    fn expand_tilde_resolves_tilde_forms_against_home() {
        let home = std::path::Path::new("/Users/x");
        assert_eq!(
            expand_tilde("~/a", home),
            std::path::PathBuf::from("/Users/x/a")
        );
        assert_eq!(expand_tilde("~", home), home.to_path_buf());
        assert_eq!(
            expand_tilde("/etc/foo", home),
            std::path::PathBuf::from("/etc/foo")
        );
    }
}
