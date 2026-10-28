// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Opt-in pane scrollback capture and replay, for `[experimental] pane_history = true`.
//!
//! `taw session save` records the layout; with this on it also records what each pane had
//! **on screen**, so `taw session restore` can paint it back before handing the pane to
//! the shell. Follows in spirit herdr's `persist::capture_history` /
//! `pane_restore_startup` (`herdr-src/src/persist/{snapshot,restore}.rs`), which store
//! the same thing in `session-history.json`.
//!
//! # Why it is off by default
//!
//! Two reasons, both worth reading before turning it on.
//!
//! **The file holds whatever was on the screen.** A pane that printed an API token, a
//! `.env`, a database row or a customer's name has that text captured into
//! `<state>/sessions/history/<name>.json` in plain text, at whatever permissions the
//! state directory has. A layout snapshot leaks nothing; this does.
//!
//! **The bytes are replayed raw.** They are captured with escape sequences intact
//! (`capture-pane -e`) so colour survives the round trip, and `cat`ing them back means
//! the terminal interprets them again — including any sequence that was on screen but is
//! not a colour. Anything a pane could have made the terminal do the first time, it can
//! do again on restore.
//!
//! # Shape
//!
//! History is stored **beside** the snapshot, not inside it, in
//! `<sessions_dir>/history/<name>.json`, and mirrors the snapshot's own structure by
//! position: sessions in order, windows in order, then panes keyed by `pane_index`. A
//! separate file, because a snapshot is small and read by `session list` on every call
//! while this is large and read only by a restore. In `history/` rather than beside it,
//! because [`super::snapshot::list`] parses every `*.json` directly in `sessions_dir` as
//! a snapshot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::tmux::Tmux;

/// Format version, matching the snapshot's.
const CURRENT_VERSION: u32 = 1;

/// How many lines above the visible screen to capture, per pane.
///
/// A bound, not a preference: `capture-pane -S -` takes the pane's whole scrollback, and
/// a `history-limit` of 50 000 lines across a dozen panes is a snapshot file of tens of
/// megabytes written on every autosave. This is a few screenfuls — enough to see what
/// the pane was doing, which is what the feature is for.
const CAPTURE_LINES: u32 = 500;

/// Every pane's screen, mirroring the snapshot's session/window order.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct History {
    pub version: u32,
    /// The fingerprint ([`super::snapshot::layout_fingerprint`]) of the snapshot these screens were
    /// taken with. A restore replays them only onto a snapshot with the same one ([`for_layout`]);
    /// a history written before there was one has none, and is never replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_fingerprint: Option<String>,
    pub sessions: Vec<SessionHistory>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SessionHistory {
    pub windows: Vec<WindowHistory>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct WindowHistory {
    /// Keyed by `#{pane_index}`, the same number [`super::snapshot::PaneSnap::index`]
    /// carries — so a pane is matched to its history by position, exactly as the restore
    /// matches a saved pane to the live one it created.
    pub panes: HashMap<u32, PaneHistory>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PaneHistory {
    /// The captured screen, escape sequences included.
    pub ansi: String,
    /// How many lines `ansi` holds — reported so a caller can see the size of what it is
    /// about to replay without decoding it.
    pub lines: usize,
}

/// Captures every pane named by `snapshot`, in the same order.
///
/// Driven by the snapshot rather than by a fresh `list-panes` so the two files always
/// line up: a pane that appeared between the two calls would otherwise shift every later
/// pane's history onto the wrong pane.
///
/// A pane that cannot be captured is left out rather than failing the save — this runs
/// unattended from autosave, and one unreadable pane must not cost the whole snapshot.
pub fn capture(t: &Tmux, snapshot: &super::snapshot::Snapshot) -> History {
    History {
        version: CURRENT_VERSION,
        layout_fingerprint: super::snapshot::layout_fingerprint(snapshot),
        sessions: snapshot
            .sessions
            .iter()
            .map(|session| SessionHistory {
                windows: session
                    .windows
                    .iter()
                    .map(|window| WindowHistory {
                        panes: window
                            .panes
                            .iter()
                            .filter_map(|pane| {
                                // The snapshot records a pane's index within its window;
                                // `<session>:<window index>.<pane index>` is the tmux
                                // target for it.
                                let target =
                                    format!("{}:{}.{}", session.name, window.index, pane.index);
                                capture_pane(t, &target).map(|history| (pane.index, history))
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// One pane's screen plus [`CAPTURE_LINES`] of scrollback, or `None` when it cannot be
/// read or has nothing on it.
fn capture_pane(t: &Tmux, target: &str) -> Option<PaneHistory> {
    let start = format!("-{CAPTURE_LINES}");
    // `-e` keeps the escape sequences, so colour survives the round trip — and is the
    // reason the replay is not merely text. `-J` is deliberately *not* used: joining
    // wrapped lines would reflow the pane's output to a width it no longer has.
    let ansi = t
        .run(&["capture-pane", "-p", "-e", "-S", &start, "-t", target])
        .ok()?;
    let trimmed = ansi.trim_end_matches(['\n', '\r']);
    if trimmed.trim().is_empty() {
        return None;
    }
    Some(PaneHistory {
        lines: trimmed.lines().count(),
        ansi: trimmed.to_string(),
    })
}

/// `<sessions_dir>/history/<name>.json`.
pub fn history_path(paths: &Paths, name: &str) -> PathBuf {
    paths
        .sessions_dir
        .join("history")
        .join(format!("{name}.json"))
}

/// Writes `history` for the snapshot called `name`, atomically.
pub fn save(paths: &Paths, name: &str, history: &History) -> anyhow::Result<PathBuf> {
    let path = history_path(paths, name);
    let dir = path
        .parent()
        .expect("history_path always has a parent directory");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string(history)
        .with_context(|| format!("serializing the pane history of '{name}'"))?;
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(path)
}

/// The history saved for `name`, or `None` when there is none.
///
/// A file that will not parse reads as "no history": the layout it belongs to is still
/// restorable, and refusing the whole restore over an unreadable *screen* would trade
/// something valuable for something cosmetic.
pub fn load(paths: &Paths, name: &str) -> Option<History> {
    let path = history_path(paths, name);
    let raw = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(history) => Some(history),
        Err(err) => {
            log::warn!("ignoring unreadable pane history {}: {err}", path.display());
            None
        }
    }
}

/// Removes the history saved for `name`, if any. Called when the feature is off, so
/// turning it off stops the screens being kept as well as being written.
pub fn clear(paths: &Paths, name: &str) -> anyhow::Result<()> {
    let path = history_path(paths, name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// Where a pane's replay text is written for the shell to `cat`:
/// `<sessions_dir>/history/<name>.replay/<window index>-<pane index>.ansi`.
fn replay_path(paths: &Paths, name: &str, window: u32, pane: u32) -> PathBuf {
    paths
        .sessions_dir
        .join("history")
        .join(format!("{name}.replay"))
        .join(format!("{window}-{pane}.ansi"))
}

/// Clears the replay directory for `name`, so a restore never `cat`s a file left over
/// from an earlier one whose layout no longer matches.
pub fn clear_replays(paths: &Paths, name: &str) {
    let dir = paths
        .sessions_dir
        .join("history")
        .join(format!("{name}.replay"));
    if let Err(err) = std::fs::remove_dir_all(&dir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not clear {}: {err}", dir.display());
    }
}

/// Writes one pane's saved screen out and returns the shell command that replays it and
/// then becomes the user's shell.
///
/// `sh -c 'cat <file>; exec "$SHELL"'` — the `exec` matters: without it the pane's real
/// shell would be a *child* of this `sh`, so `exit` would drop the user into the wrapper
/// instead of closing the pane, and `#{pane_current_command}` would name `sh` rather than
/// what the pane is running, which is one of the columns the detection engine reads.
///
/// `$SHELL` is taken from the pane's own environment at that moment, with `/bin/sh` as
/// the fallback, so a user's login shell is what they get.
///
/// Returns `None` when the history cannot be written out — the pane then restores as a
/// plain shell, which is what it would have been without this feature at all.
pub fn replay_command(
    paths: &Paths,
    name: &str,
    window: u32,
    pane: u32,
    history: &PaneHistory,
) -> Option<String> {
    let path = replay_path(paths, name, window, pane);
    let dir = path.parent()?;
    if let Err(err) = std::fs::create_dir_all(dir) {
        log::warn!("could not create {}: {err}", dir.display());
        return None;
    }
    // A trailing newline so the shell's first prompt starts on its own line rather than
    // running on from the last captured one.
    if let Err(err) = std::fs::write(&path, format!("{}\n", history.ansi)) {
        log::warn!("could not write {}: {err}", path.display());
        return None;
    }
    Some(format!(
        "cat {} 2>/dev/null; exec \"${{SHELL:-/bin/sh}}\"",
        shell_words::quote(&path.to_string_lossy())
    ))
}

/// One pane's saved history from a loaded [`History`], addressed the way the restore
/// addresses it: session position, window position, pane index.
pub fn pane_history(
    history: &History,
    session_index: usize,
    window_index: usize,
    pane_index: u32,
) -> Option<&PaneHistory> {
    history
        .sessions
        .get(session_index)?
        .windows
        .get(window_index)?
        .panes
        .get(&pane_index)
}

// Follows herdr's src/persist/restore.rs, the history check at :271-278.
// Differences: the snapshot is named in the warning, and a history is a file of its own per name.
/// `history` when it was taken with the layout `snapshot` holds, which is when it records a
/// fingerprint and the fingerprint is the snapshot's; otherwise `None`, with a warning naming
/// `name`. So a snapshot recovered by hand, or one whose history another save has replaced, never
/// gets screens that were another layout's, and a history written before it recorded a fingerprint
/// is not replayed (the next save writes one that does).
pub fn for_layout(
    history: History,
    snapshot: &super::snapshot::Snapshot,
    name: &str,
) -> Option<History> {
    let ours = super::snapshot::layout_fingerprint(snapshot);
    if history.layout_fingerprint.is_some() && history.layout_fingerprint == ours {
        return Some(history);
    }
    log::warn!(
        "not replaying the saved screens of '{name}': they were not taken with the layout of the \
         snapshot being restored"
    );
    None
}

/// Whether `path` is inside `paths.sessions_dir` — the guard on what a restore is willing
/// to `cat` into a pane.
pub fn is_under_sessions_dir(paths: &Paths, path: &Path) -> bool {
    path.starts_with(&paths.sessions_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(dir: &Path) -> Paths {
        let plugins_dir = dir.join("plugins");
        Paths {
            config_dir: dir.join("config"),
            config_file: dir.join("config").join("config.toml"),
            state_dir: dir.join("state"),
            override_dir: dir.join("config").join("agent-detection"),
            remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
            sessions_dir: dir.join("state").join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: dir.join("state").join("logs"),
            manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
        }
    }

    fn sample() -> History {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneHistory {
                ansi: "\u{1b}[32mgreen\u{1b}[0m\nsecond line".to_string(),
                lines: 2,
            },
        );
        History {
            version: 1,
            layout_fingerprint: None,
            sessions: vec![SessionHistory {
                windows: vec![WindowHistory { panes }],
            }],
        }
    }

    fn a_snapshot() -> super::super::snapshot::Snapshot {
        use super::super::snapshot::{PaneSnap, SessionSnap, Snapshot, WindowSnap};
        Snapshot {
            version: 1,
            taken_unix: 1_700_000_000,
            skipped_panes: 0,
            sessions: vec![SessionSnap {
                name: "alpha".to_string(),
                path: "/repo".to_string(),
                active_window_index: 0,
                windows: vec![WindowSnap {
                    index: 0,
                    name: "editor".to_string(),
                    layout: "abcd,80x24,0,0,0".to_string(),
                    active: true,
                    panes: vec![PaneSnap {
                        index: 0,
                        cwd: "/repo".to_string(),
                        active: true,
                        agent_kind: None,
                        agent_session_id: None,
                        agent_session_path: None,
                        agent_name: None,
                        agent_kind_hint: None,
                        agent_resume: None,
                        agent_sandbox: None,
                        agent_container: None,
                        agent_claude_flags: Vec::new(),
                    }],
                }],
            }],
        }
    }

    /// herdr replays screen history only when its fingerprint is present and the restored
    /// snapshot's: the layout it was taken with, at any time, is replayed onto; another layout, a
    /// history with no fingerprint and a snapshot that has none to compare are not.
    #[test]
    fn a_history_is_replayed_only_onto_the_layout_it_was_taken_with() {
        let snapshot = a_snapshot();
        let taken_with = History {
            layout_fingerprint: super::super::snapshot::layout_fingerprint(&snapshot),
            ..sample()
        };
        assert!(taken_with.layout_fingerprint.is_some());
        assert_eq!(
            for_layout(taken_with.clone(), &snapshot, "alpha"),
            Some(taken_with.clone())
        );
        let later = super::super::snapshot::Snapshot {
            taken_unix: snapshot.taken_unix + 600,
            ..snapshot.clone()
        };
        assert_eq!(
            for_layout(taken_with.clone(), &later, "alpha"),
            Some(taken_with.clone()),
            "the time a snapshot was taken is no part of its layout"
        );

        let mut moved = snapshot.clone();
        moved.sessions[0].windows[0].layout = "efgh,80x24,0,0,1".to_string();
        assert_eq!(for_layout(taken_with.clone(), &moved, "alpha"), None);

        let before_fingerprints = History {
            layout_fingerprint: None,
            ..taken_with.clone()
        };
        assert_eq!(for_layout(before_fingerprints, &snapshot, "alpha"), None);
    }

    /// A history file written before there was a fingerprint reads back with none.
    #[test]
    fn a_history_file_without_a_fingerprint_loads_with_none_and_one_with_it_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let old = r#"{"version":1,"sessions":[]}"#;
        std::fs::create_dir_all(history_path(&paths, "old").parent().unwrap()).unwrap();
        std::fs::write(history_path(&paths, "old"), old).unwrap();
        assert_eq!(load(&paths, "old").unwrap().layout_fingerprint, None);

        let with = History {
            layout_fingerprint: Some("abc".to_string()),
            ..sample()
        };
        save(&paths, "new", &with).unwrap();
        assert_eq!(load(&paths, "new").unwrap(), with);
        let raw = std::fs::read_to_string(history_path(&paths, "new")).unwrap();
        assert!(raw.contains("\"layout_fingerprint\":\"abc\""), "{raw}");
        save(&paths, "plain", &sample()).unwrap();
        let raw = std::fs::read_to_string(history_path(&paths, "plain")).unwrap();
        assert!(!raw.contains("layout_fingerprint"), "{raw}");
    }

    #[test]
    fn history_lives_beside_the_snapshot_not_where_session_list_would_parse_it() {
        // `snapshot::list` parses every `*.json` directly in `sessions_dir` as a snapshot,
        // so a history file next to one would break `taw session list` outright.
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let path = history_path(&paths, "default");
        assert_eq!(path.parent().unwrap(), paths.sessions_dir.join("history"));
        assert_eq!(path.file_name().unwrap(), "default.json");
    }

    #[test]
    fn history_round_trips_through_save_and_load_with_its_escape_sequences() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let history = sample();
        let path = save(&paths, "default", &history).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(load(&paths, "default").unwrap(), history);
        // Colour survived, which is the whole reason `-e` is used on capture.
        assert!(
            load(&paths, "default").unwrap().sessions[0].windows[0].panes[&0]
                .ansi
                .contains("\u{1b}[32m")
        );
    }

    #[test]
    fn a_missing_or_unreadable_history_reads_as_none_rather_than_failing_a_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        assert!(load(&paths, "default").is_none());

        let path = history_path(&paths, "broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(
            load(&paths, "broken").is_none(),
            "an unreadable screen must not cost the layout"
        );
    }

    #[test]
    fn clear_removes_the_file_and_is_a_no_op_when_there_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        clear(&paths, "default").unwrap();
        assert!(!history_path(&paths, "default").exists());
        // Turning the feature off runs this on a server that never had history.
        clear(&paths, "default").unwrap();
    }

    #[test]
    fn pane_history_is_addressed_by_position_then_pane_index() {
        let history = sample();
        assert!(pane_history(&history, 0, 0, 0).is_some());
        // Nothing saved for a pane index, window or session the history does not hold.
        assert!(pane_history(&history, 0, 0, 1).is_none());
        assert!(pane_history(&history, 0, 1, 0).is_none());
        assert!(pane_history(&history, 1, 0, 0).is_none());
    }

    #[test]
    fn the_replay_command_execs_the_users_shell_after_catting_the_saved_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let history = PaneHistory {
            ansi: "hello".to_string(),
            lines: 1,
        };
        let cmd = replay_command(&paths, "default", 0, 1, &history).unwrap();

        assert!(cmd.starts_with("cat "), "{cmd}");
        // `exec`, not a plain invocation: a shell running as a child of the wrapper would
        // make `exit` drop the user into `sh` instead of closing the pane, and
        // `#{pane_current_command}` would report `sh` rather than what is running — a
        // column the detection engine reads.
        assert!(cmd.contains("exec \"${SHELL:-/bin/sh}\""), "{cmd}");

        let path = replay_path(&paths, "default", 0, 1);
        assert!(cmd.contains(&path.to_string_lossy().to_string()), "{cmd}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        assert!(
            is_under_sessions_dir(&paths, &path),
            "a replay file must live under the sessions dir"
        );
    }

    #[test]
    fn a_replay_path_with_shell_syntax_in_it_is_quoted() {
        // The state directory is `$TAW_STATE_DIR`, which a user can point anywhere —
        // including at a path holding a space or a `$`. The command string is executed by
        // a shell, so the path has to survive it.
        let tmp = tempfile::tempdir().unwrap();
        let awkward = tmp.path().join("state dir $(whoami)");
        let paths = test_paths(&awkward);
        let history = PaneHistory {
            ansi: "x".to_string(),
            lines: 1,
        };
        let cmd = replay_command(&paths, "default", 0, 0, &history).unwrap();
        assert!(
            cmd.contains('\'') || !cmd.contains("$(whoami)"),
            "the path was not quoted: {cmd}"
        );
        // And the file really is where the command says.
        let quoted = cmd
            .strip_prefix("cat ")
            .and_then(|rest| rest.split(" 2>/dev/null").next())
            .unwrap();
        let unquoted = shell_words::split(quoted).unwrap();
        assert!(Path::new(&unquoted[0]).exists(), "{cmd}");
    }

    #[test]
    fn clear_replays_removes_a_stale_replay_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let history = PaneHistory {
            ansi: "old".to_string(),
            lines: 1,
        };
        replay_command(&paths, "default", 0, 0, &history).unwrap();
        let dir = replay_path(&paths, "default", 0, 0)
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(dir.exists());
        clear_replays(&paths, "default");
        assert!(
            !dir.exists(),
            "a restore must not cat a file left by an earlier one"
        );
        // And on a server that never restored with history on.
        clear_replays(&paths, "default");
    }
}
