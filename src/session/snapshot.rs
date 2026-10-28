// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Capturing and persisting the tmux layout (sessions/windows/panes) plus each pane's
//! agent identity.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::tmux::{PaneInfo, Tmux};

/// The format this taw writes: 2 since sandboxed agents, whose panes carry the profile their agent
/// was started through and the runtime it ran under. A taw from before, which knows nothing of
/// either, refuses the file as it refuses any newer one, rather than resuming a sandboxed agent on
/// this machine.
const CURRENT_VERSION: u32 = 2;

/// Whether a reader of formats up to `supported` can use a snapshot of format `found`.
fn readable_by(found: u32, supported: u32) -> bool {
    found <= supported
}

// Follows herdr's src/persist/io.rs. Differences: a snapshot from a
// newer format is refused with a typed error, where herdr logs it and ignores the file.
/// Why a saved session cannot be used, as a matchable error rather than a message only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    /// A newer taw wrote it, in a format this one does not read (herdr's
    /// `unsupported_version`): restoring it here would rebuild the layout without whatever
    /// the newer format added, so it is refused and the file is left as it is.
    #[error(
        "saved session '{name}' was written by a newer taw (snapshot format {found}, this taw \
         reads up to {supported}), so it was left as it is; update taw to restore it"
    )]
    NewerFormat {
        name: String,
        found: u32,
        supported: u32,
    },
}

/// A full point-in-time capture of the tmux server: every session, its windows and panes,
/// plus the agent identity each pane carries. See [`take`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub taken_unix: u64,
    pub sessions: Vec<SessionSnap>,
    /// How many panes tmux reported that could not be parsed and are therefore *missing*
    /// from this snapshot. Normally 0. Carried so `session save` can say the snapshot is
    /// incomplete instead of reporting success for a layout that restore would then
    /// rebuild wrong. `#[serde(default)]`: snapshots written before this field existed
    /// read back as 0.
    #[serde(default)]
    pub skipped_panes: usize,
}

/// One tmux session (herdr workspace): its name, cwd, and windows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionSnap {
    pub name: String,
    pub path: String,
    pub active_window_index: u32,
    pub windows: Vec<WindowSnap>,
}

/// One tmux window (herdr tab): its saved `#{window_layout}` string, so restore can
/// reproduce the exact pane geometry via `select-layout`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WindowSnap {
    pub index: u32,
    pub name: String,
    pub layout: String,
    pub active: bool,
    pub panes: Vec<PaneSnap>,
}

// Follows herdr's src/persist/snapshot.rs, `PaneSnapshot::agent_resume`: the same field,
// read here from the pane's three options.
/// One tmux pane and, when an agent occupies it, enough of its identity for
/// `session::restore` to resume that agent's native conversation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PaneSnap {
    pub index: u32,
    pub cwd: String,
    pub active: bool,
    pub agent_kind: Option<String>,
    pub agent_session_id: Option<String>,
    pub agent_session_path: Option<String>,
    pub agent_name: Option<String>,
    pub agent_kind_hint: Option<String>,
    /// The command the agent reported that resumes its session ([`crate::agent_resume`]), which a
    /// restore types in place of taw's own. Absent from snapshots written before there was one, which
    /// load as having none, and left out of a pane that has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_resume: Option<crate::agent_resume::ReportedResume>,
    /// The sandbox profile its agent was started through (`@agent_sandbox`): a restore starts the
    /// agent again through that profile, and never outside it. Absent from snapshots written before
    /// format 2, and from a pane kept by a restore before then (`@agent_restore_saved`), which load
    /// as having none, and left out of a pane that has none, whose JSON stays as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_sandbox: Option<String>,
    /// The container runtime its agent ran under (`@agent_container`), whoever started it. Absent and
    /// left out as `agent_sandbox` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_container: Option<String>,
    /// The flags its Claude was started with that set how the session runs (`--effort max`,
    /// [`crate::claude_flags`]), from the pane's `@agent_claude_flags`: a restore types them after
    /// `claude`. Absent from snapshots written before there were any, and from a pane kept by a restore
    /// before then (`@agent_restore_saved`), which load as having none, and left out of a pane that has
    /// none, whose JSON stays as it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_claude_flags: Vec<String>,
}

/// What a restore resumed a sandboxed agent with, kept on its pane while its profile starts
/// (`store::SANDBOX_RESUME_OPTION`): what a save needs to resume it again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeptResume {
    pub kind: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_path: Option<String>,
    /// The command the agent reported for itself, when the restore resumed from one.
    #[serde(default)]
    pub agent_resume: Option<crate::agent_resume::ReportedResume>,
}

/// A marked pane's snapshot, with what its restore kept (`kept`, a [`KeptResume`] as JSON): the
/// kind, the session and the reported command come from it while the pane's record holds no
/// session of its own, the kind and the command always with the session (a record's other kind or
/// command beside the kept session would pair one agent's kind, or its way of resuming, with
/// another's conversation). The record's own wins once there is one; a pane with no mark keeps
/// nothing.
pub(crate) fn with_kept_resume(mut pane: PaneSnap, kept: Option<&str>) -> PaneSnap {
    if pane.agent_sandbox.is_none()
        || pane.agent_session_id.is_some()
        || pane.agent_session_path.is_some()
    {
        return pane;
    }
    let Some(kept) = kept.and_then(|text| serde_json::from_str::<KeptResume>(text).ok()) else {
        return pane;
    };
    pane.agent_kind = Some(kept.kind);
    pane.agent_session_id = kept.session_id;
    pane.agent_session_path = kept.session_path;
    pane.agent_resume = kept.agent_resume;
    pane
}

// Follows herdr's src/persist/snapshot.rs, `layout_fingerprint`.
// Differences: `taken_unix` is left out, since taw stamps it on every save where herdr's
// snapshot has no such field; herdr's set-order step is not needed, taw's snapshot holds no sets.
/// A fingerprint of the layout `snapshot` holds: the sha256 of its JSON value with `taken_unix`
/// left out, in hex. Two snapshots of one layout taken at different times have the same one, and
/// so do two files with the same layout written with other key order or whitespace, since it is
/// read off the struct and not off the file's bytes. The format `version` counts, as every other
/// field but `taken_unix` does, as in herdr's, which covers the whole snapshot: a snapshot saved
/// under another format is another layout to the copies and to a screen history. `None` only if
/// the snapshot cannot be turned to JSON.
pub fn layout_fingerprint(snapshot: &Snapshot) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut value = serde_json::to_value(snapshot).ok()?;
    value.as_object_mut()?.shift_remove("taken_unix");
    let bytes = serde_json::to_vec(&value).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// Captures every session/window/pane on the server, plus each pane's agent identity, in
/// one `list_panes` call (it already carries session/window/pane fields, the window
/// layout, and the `@agent_*` options — see `Tmux::list_panes`). Panes are grouped by
/// session then by window, in the order `list_panes` returns them; within each session,
/// windows are sorted ascending by `window_index` and each window's panes ascending by
/// `pane_index`, so a snapshot's window/pane order always matches tmux's own creation
/// order — the order `session::restore::apply` recreates them in.
///
/// A pane that a restore left as tmux made it, its saved directory being gone, is taken as it
/// was saved, until it is closed or an agent of its own is recorded in it ([`kept_by_a_restore`]).
pub fn take(t: &Tmux) -> anyhow::Result<Snapshot> {
    // The counting variant: a pane that did not parse is a pane missing from what this
    // writes to disk, and a snapshot that quietly lost one is worse than a loud one.
    // Deliberately not an error — autosave runs unattended, and one bad pane must not
    // stop every later snapshot for ever.
    let (panes, skipped_panes) = t.list_panes_reporting_skipped()?;
    if skipped_panes > 0 {
        log::warn!(
            "snapshot is missing {skipped_panes} pane(s) tmux reported but could not be read"
        );
    }

    let mut order: Vec<String> = Vec::new();
    let mut sessions: HashMap<String, SessionAcc> = HashMap::new();

    for pane in &panes {
        let acc = sessions.entry(pane.session_id.clone()).or_insert_with(|| {
            order.push(pane.session_id.clone());
            SessionAcc {
                name: pane.session_name.clone(),
                path: pane.session_path.clone(),
                active_window_index: 0,
                windows_by_id: HashMap::new(),
                windows: Vec::new(),
            }
        });

        if pane.window_active {
            acc.active_window_index = pane.window_index;
        }

        let window_idx = *acc
            .windows_by_id
            .entry(pane.window_id.clone())
            .or_insert_with(|| {
                acc.windows.push(WindowSnap {
                    index: pane.window_index,
                    name: pane.window_name.clone(),
                    layout: pane.window_layout.clone(),
                    active: pane.window_active,
                    panes: Vec::new(),
                });
                acc.windows.len() - 1
            });

        acc.windows[window_idx].panes.push(saved_pane(t, pane));
    }

    let mut sessions_out: Vec<SessionSnap> = order
        .into_iter()
        .filter_map(|id| sessions.remove(&id))
        .map(|mut acc| {
            acc.windows.sort_by_key(|w| w.index);
            for window in &mut acc.windows {
                window.panes.sort_by_key(|p| p.index);
            }
            SessionSnap {
                name: acc.name,
                path: acc.path,
                active_window_index: acc.active_window_index,
                windows: acc.windows,
            }
        })
        .collect();
    // Deterministic top-level order (list_panes' own session order is otherwise whatever
    // tmux happens to return it in).
    sessions_out.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Snapshot {
        version: CURRENT_VERSION,
        taken_unix: now_unix(),
        sessions: sessions_out,
        skipped_panes,
    })
}

struct SessionAcc {
    name: String,
    path: String,
    active_window_index: u32,
    windows_by_id: HashMap<String, usize>,
    windows: Vec<WindowSnap>,
}

/// The snapshot of `pane`: [`pane_snap`], and, for a pane marked with a profile whose record holds no
/// session, what a restore kept of the agent it resumes there ([`with_kept_resume`]), while the
/// profile starts. No other pane costs a tmux call.
pub(crate) fn saved_pane(t: &Tmux, pane: &PaneInfo) -> PaneSnap {
    let snap = pane_snap(pane);
    if snap.agent_sandbox.is_none()
        || snap.agent_session_id.is_some()
        || snap.agent_session_path.is_some()
    {
        return snap;
    }
    // Read raw from a format: a listing of the pane's options quotes what JSON holds.
    let kept = t
        .display(
            &pane.pane_id,
            &format!("#{{{}}}", crate::store::SANDBOX_RESUME_OPTION),
        )
        .ok()
        .filter(|kept| !kept.trim().is_empty());
    with_kept_resume(snap, kept.as_deref())
}

fn pane_snap(pane: &PaneInfo) -> PaneSnap {
    // A pane a restore kept as it was saved keeps all it was saved with, its profile and runtime
    // included, whatever runs in it now.
    kept_by_a_restore(pane).unwrap_or_else(|| PaneSnap {
        index: pane.pane_index,
        cwd: pane.pane_current_path.clone(),
        active: pane.pane_active,
        agent_kind: pane.agent.kind.clone(),
        agent_session_id: pane.agent.session_id.clone(),
        agent_session_path: pane.agent.session_path.clone(),
        agent_name: pane.agent.name.clone(),
        agent_kind_hint: pane.agent.kind_hint.clone(),
        agent_resume: pane.agent.resume.clone(),
        agent_sandbox: pane.agent.sandbox.clone(),
        agent_container: runtime_of(pane),
        agent_claude_flags: pane.agent.claude_flags.clone(),
    })
}

/// The container runtime a save keeps beside a pane's session: the one the watcher marked the pane
/// with (`@agent_container`), or, for a pane with a session to resume, a runtime in front of it by its
/// command, which the watcher has not read yet (from the moment it came, or while a report held its
/// mark back to the next read). A restore resumes a pane saved with a runtime only through the
/// profile saved with it, and one saved with a runtime and no profile not at all
/// (`restore::plan_of`), so no session a save keeps is ever typed outside its container.
fn runtime_of(pane: &PaneInfo) -> Option<String> {
    if pane.agent.container.is_some() {
        return pane.agent.container.clone();
    }
    let resumable = pane.agent.session_id.is_some()
        || pane.agent.session_path.is_some()
        || pane.agent.resume.is_some();
    resumable
        .then(|| crate::detect::container_runtime(&pane.pane_current_command))
        .flatten()
        .map(str::to_string)
}

// Follows herdr's src/persist/restore.rs, what a pane it could not restore
// keeps for the next capture (`unavailable_restored_terminal`), and its test
// `failed_cold_restore_preserves_panes_and_saved_directories` ("fallback cwd must not replace saved
// intent"). Differences: tmux has made the pane, in the home folder, so the pane as it was saved is
// read back from the pane option the restore left it in, and it is kept until the pane is closed or
// an agent of its own is recorded in it (a pane here has a shell, which an agent can start in), where
// herdr's has none and keeps its saved directory until it is closed.
/// The pane as it was saved, for a pane that a restore left as tmux made it because its saved
/// directory was gone (`@agent_restore_saved`): its directory, its agent, session and name, and the
/// command its agent reported, which a save keeps in place of the home folder tmux opened the pane in
/// and of what the pane holds now, until an agent of the pane's own is recorded in it, which takes the
/// option off (`store::RESTORE_OPTIONS`); with the pane's place in its window, and whether it is the
/// window's active pane, as they are now. `None` for every other pane, and for a value that does not
/// read as a saved pane.
pub(crate) fn kept_by_a_restore(pane: &PaneInfo) -> Option<PaneSnap> {
    let kept: PaneSnap = serde_json::from_str(pane.agent.restore_saved.as_deref()?).ok()?;
    Some(PaneSnap {
        index: pane.pane_index,
        active: pane.pane_active,
        ..kept
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `<sessions_dir>/<name>.json`.
pub(crate) fn snapshot_path(paths: &Paths, name: &str) -> PathBuf {
    paths.sessions_dir.join(format!("{name}.json"))
}

/// Writes `snap` to `<sessions_dir>/<name>.json` atomically (`.tmp` file, then rename over
/// the real path), pretty-printed. Returns the path written.
///
/// If the file already there cannot be parsed as a [`Snapshot`] — a newer taw wrote it, or
/// it is damaged — it is copied into `session-backups/` first (see
/// [`preserve_unreadable_existing`]), so replacing it never discards the only copy of
/// whatever it held. If that copy fails, the save is skipped and the original is left
/// exactly as it was.
pub fn save(paths: &Paths, name: &str, snap: &Snapshot) -> anyhow::Result<PathBuf> {
    save_for_server(paths, name, snap, None)
}

/// [`save`] on a tmux server that started at `server_start` (unix seconds, tmux's `#{start_time}`),
/// when there is one to ask. A snapshot this replaces that was taken before the server started is
/// copied into `session-snapshots/` first, however new the newest copy is ([`copy_is_due`]): it is
/// the layout of a server that is gone, which the one that started since has not had a chance to
/// restore, and the save is about to replace it. `None` keeps the rule of [`save`].
pub fn save_for_server(
    paths: &Paths,
    name: &str,
    snap: &Snapshot,
    server_start: Option<u64>,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(&paths.sessions_dir)
        .with_context(|| format!("creating {}", paths.sessions_dir.display()))?;
    preserve_unreadable_existing(paths, name, now_nanos())?;
    preserve_snapshot_history(paths, name, server_start);
    let path = snapshot_path(paths, name);
    let tmp = staging_path(paths, name);
    let json = serde_json::to_string_pretty(snap)
        .with_context(|| format!("serializing snapshot '{name}'"))?;
    // Whichever step fails, the staging file goes: a write that dies partway (a full disk)
    // leaves the part it wrote, and a rename that fails leaves all of it. Once the rename
    // has worked there is nothing of it left to remove.
    let staged = std::fs::write(&tmp, json)
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
        });
    if let Err(err) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    preserve_snapshot_history(paths, name, None);
    Ok(path)
}

/// Where a save of `name` stages its bytes before renaming them over the snapshot: a name
/// of its own for every save (this process's id and a counter), so two saves of one name
/// (several `session-closed` hooks whose waits end together) cannot write one file or
/// rename each other's away. It ends in `.tmp`, so it never looks like a saved snapshot
/// (`snapshot_files` lists `.json`).
fn staging_path(paths: &Paths, name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    paths
        .sessions_dir
        .join(format!("{name}.json.{}-{sequence}.tmp", std::process::id()))
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// Follows herdr's src/persist/writer.rs. Differences: one call per save
// that checks the existing file's own readability, rather than a flag a long-lived
// SessionWriter carries between saves, and backups are kept per snapshot name.
/// If `<sessions_dir>/<name>.json` exists and does not parse as a [`Snapshot`], copies it
/// into `<sessions_dir>/session-backups/session-<name>-<now_nanos:039>-<pid>-<seq>.json`
/// before [`save`] replaces it, then prunes that name's backups to the three newest. A
/// healthy (or absent) file is left alone — this does nothing, and nothing is copied.
///
/// `now_nanos` is passed in, not read from the clock here, so creation order can be
/// asserted even across a wall-clock rollback (herdr's own test for this): each new
/// backup's encoded timestamp is at least one more than the previous highest for this
/// name, whatever `now_nanos` says.
///
/// Follows herdr's `persist/writer.rs` `preserve_existing`/`copy_recovery`/
/// `prune_backups`, folded into one call per save rather than a flag a long-lived
/// process's `SessionWriter` carries between saves — taw has no such process to carry it
/// in, and checking the file itself on every save is exactly as cheap.
fn preserve_unreadable_existing(paths: &Paths, name: &str, now_nanos: u128) -> anyhow::Result<()> {
    let path = snapshot_path(paths, name);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    if is_readable(&raw) {
        return Ok(());
    }

    copy_recovery(
        paths,
        name,
        BACKUPS_DIR,
        BACKUPS_KEPT,
        &raw,
        now_nanos,
        PruneFailure::Ignored,
    )?;
    Ok(())
}

/// What a failed prune of the oldest copies does to the copy just made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PruneFailure {
    /// Logged, the copy stays (the backups of an unreadable file: more than asked for is safe).
    Ignored,
    /// The copy just made is removed and the failure given back (herdr's rule for snapshots, so a
    /// directory that cannot be pruned does not grow by a copy on every save).
    RemovesTheCopy,
}

const BACKUPS_DIR: &str = "session-backups";
const BACKUPS_KEPT: usize = 3;
const SNAPSHOTS_DIR: &str = "session-snapshots";
/// How many copies of one name `session-snapshots/` keeps (herdr's `SNAPSHOT_LIMIT`).
const SNAPSHOTS_KEPT: usize = 48;
/// How old the newest copy of a name has to be before another is made (herdr's `SNAPSHOT_INTERVAL`).
const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

// Follows herdr's src/persist/writer.rs, `preserve_existing_in`,
// `copy_recovery` and `prune_backups`. Differences: the bytes are read once by the caller, the
// copies of one name are told apart by the name in the file's name, and the directory and the
// number kept are parameters, as herdr's are.
/// Writes `raw` as the next copy of `name` in `<sessions_dir>/<directory>/`, named
/// `session-<name>-<timestamp:039>-<pid>-<seq>.json`, and prunes that name's copies to the `keep`
/// newest. The timestamp is `now_nanos`, or one more than the newest copy's when the clock went
/// back, so creation order survives a rollback. A copy is never overwritten: a name that is taken
/// gives the next sequence number.
fn copy_recovery(
    paths: &Paths,
    name: &str,
    directory: &str,
    keep: usize,
    raw: &[u8],
    now_nanos: u128,
    on_prune_failure: PruneFailure,
) -> anyhow::Result<PathBuf> {
    let dir = paths.sessions_dir.join(directory);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let prefix = format!("session-{name}-");
    let older = backup_files(&dir, &prefix)?;

    // Keep creation order even when the wall clock moves backwards.
    let timestamp = match older.last() {
        Some((previous, _)) => now_nanos.max(previous.saturating_add(1)),
        None => now_nanos,
    };
    for sequence in 0..128 {
        let copy = dir.join(format!(
            "{prefix}{timestamp:039}-{}-{sequence}.json",
            std::process::id()
        ));
        match write_new_copy(&copy, raw) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).with_context(|| format!("writing {}", copy.display())),
        }
        if let Err(err) = prune_copies(&older, keep) {
            match on_prune_failure {
                PruneFailure::RemovesTheCopy => {
                    let _ = std::fs::remove_file(&copy);
                    return Err(err).with_context(|| format!("pruning {}", dir.display()));
                }
                PruneFailure::Ignored => {
                    log::warn!("could not prune the copies in {}: {err}", dir.display());
                }
            }
        }
        return Ok(copy);
    }
    Err(anyhow::anyhow!(
        "could not allocate a copy of '{name}' in {}",
        dir.display()
    ))
}

/// Writes `raw` to a pending file beside `copy`, then renames it into place, so a copy that is
/// there is whole. `AlreadyExists` when `copy` (or its pending file) is there already.
fn write_new_copy(copy: &std::path::Path, raw: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let pending = copy.with_extension("pending");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    let written = (|| {
        if std::fs::symlink_metadata(copy).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "copy already exists",
            ));
        }
        output.write_all(raw)?;
        output.sync_all()?;
        std::fs::rename(&pending, copy)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&pending);
    }
    written
}

/// Removes the oldest of `older` so that, with one copy just made, `keep` remain. An entry that
/// cannot be removed is passed over, and the next one goes in its place; the failure is given
/// back only if too many are left in the end.
fn prune_copies(older: &[(u128, PathBuf)], keep: usize) -> std::io::Result<()> {
    let mut remaining = older.len().saturating_sub(keep.saturating_sub(1));
    let mut failure = None;
    for (_, path) in older {
        if remaining == 0 {
            return Ok(());
        }
        match std::fs::remove_file(path) {
            Ok(()) => remaining -= 1,
            // Another save pruned it first: it is gone, which is what was wanted.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => remaining -= 1,
            Err(err) => failure = Some(err),
        }
    }
    match failure {
        Some(err) if remaining > 0 => Err(err),
        _ => Ok(()),
    }
}

// Follows herdr's src/persist/writer.rs, `preserve_snapshot_history`.
// Differences: one name's copies only (its prefix in the file name), the file read once, and the
// time passed in for the copy's name.
/// Copies `<sessions_dir>/<name>.json`, as it is now, into `session-snapshots/`, unless the newest
/// copy of `name` is under 15 minutes old (one dated in the future does not count: the clock went
/// back), the file is not there, does not parse, was written by a newer taw or holds no session,
/// or its layout ([`layout_fingerprint`]) is the newest copy's. The 48 newest copies are kept.
/// A file taken before the tmux server started (`server_start`) is copied whatever the age of the
/// newest copy ([`copy_is_due`]). Run by [`save`] before it replaces the file and again after, and
/// by [`delete`] before it removes it, so a first save is copied at once, a quick burst adds none,
/// and a layout that did not change is never copied twice. A copy that fails is logged and what
/// called it goes ahead.
fn preserve_snapshot_history(paths: &Paths, name: &str, server_start: Option<u64>) {
    if let Err(err) = copy_into_history(paths, name, now_nanos(), server_start) {
        log::warn!("could not keep a copy of the saved session '{name}': {err:#}");
    }
}

/// Whether a copy of a snapshot is made now: `newest_copy_age` is the age of the newest copy of the
/// name (`None` when there is none, or it is dated in the future), `taken_unix` is when the snapshot
/// to copy was taken, `server_start` is when the tmux server started, when it is known, and
/// `same_layout` says the snapshot has the newest copy's layout.
///
/// A copy of a layout the newest copy already has is never made. Otherwise a copy is made when there
/// is none, when the newest is [`SNAPSHOT_INTERVAL`] old or more, or, whatever the newest's age, when
/// the snapshot was taken strictly before the server started: it is the last layout of a server that
/// is gone, and the save that is about to replace it is the first of the new one. With no server
/// start known, only the first two apply.
pub(crate) fn copy_is_due(
    newest_copy_age: Option<std::time::Duration>,
    taken_unix: u64,
    server_start: Option<u64>,
    same_layout: bool,
) -> bool {
    if same_layout {
        return false;
    }
    let from_before_the_server = server_start.is_some_and(|start| taken_unix < start);
    from_before_the_server || newest_copy_age.is_none_or(|age| age >= SNAPSHOT_INTERVAL)
}

fn copy_into_history(
    paths: &Paths,
    name: &str,
    now_nanos: u128,
    server_start: Option<u64>,
) -> anyhow::Result<()> {
    let dir = paths.sessions_dir.join(SNAPSHOTS_DIR);
    let existing = match backup_files(&dir, &format!("session-{name}-")) {
        Ok(files) => files,
        Err(err) if is_not_found(&err) => Vec::new(),
        Err(err) => return Err(err),
    };
    let newest_copy_age = match existing.last() {
        Some((_, newest)) => {
            let modified = std::fs::metadata(newest)
                .and_then(|meta| meta.modified())
                .with_context(|| format!("reading the time of {}", newest.display()))?;
            // Dated in the future: the clock went back, and that copy is not a recent one.
            std::time::SystemTime::now().duration_since(modified).ok()
        }
        None => None,
    };
    let path = snapshot_path(paths, name);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&raw) else {
        return Ok(());
    };
    if !readable_by(snapshot.version, CURRENT_VERSION) || snapshot.sessions.is_empty() {
        return Ok(());
    }
    let mut same_layout = false;
    if let Some((_, newest)) = existing.last() {
        let previous =
            std::fs::read(newest).with_context(|| format!("reading {}", newest.display()))?;
        if let Ok(previous) = serde_json::from_slice::<Snapshot>(&previous)
            && layout_fingerprint(&snapshot)
                .is_some_and(|now| layout_fingerprint(&previous).as_ref() == Some(&now))
        {
            same_layout = true;
        }
    }
    if !copy_is_due(
        newest_copy_age,
        snapshot.taken_unix,
        server_start,
        same_layout,
    ) {
        return Ok(());
    }
    copy_recovery(
        paths,
        name,
        SNAPSHOTS_DIR,
        SNAPSHOTS_KEPT,
        &raw,
        now_nanos,
        PruneFailure::RemovesTheCopy,
    )?;
    Ok(())
}

/// Whether `err` is, at its root, a missing file or directory.
fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Whether this taw can use `raw` as a saved layout: it parses, and it was not written in a
/// newer format. `Snapshot` ignores fields it does not know, so a newer taw's file that only
/// adds some parses fine; the version is what says this build should not overwrite it
/// without keeping a copy, or restore it (herdr treats `version > SNAPSHOT_VERSION` as
/// unloadable the same way).
fn is_readable(raw: &[u8]) -> bool {
    serde_json::from_slice::<Snapshot>(raw)
        .is_ok_and(|snap| readable_by(snap.version, CURRENT_VERSION))
}

/// Every backup file in `dir` whose name starts with `prefix`, as `(timestamp, path)`,
/// sorted by timestamp ascending (oldest first).
fn backup_files(dir: &std::path::Path, prefix: &str) -> anyhow::Result<Vec<(u128, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(timestamp) = backup_timestamp(name, prefix) {
            files.push((timestamp, entry.path()));
        }
    }
    files.sort();
    Ok(files)
}

/// The timestamp in a backup's file name, or `None` if `file_name` is not a backup of the
/// snapshot `prefix` (`session-<name>-`) stands for.
///
/// Everything after the prefix has to be exactly `<39 digits>-<digits>-<digits>.json`, as
/// herdr's `recovery_timestamp` requires of its own names. Reading only up to the first
/// dash would take a backup of `proj-2024` (`session-proj-2024-...`) for one of `proj`,
/// with `2024` as its timestamp.
fn backup_timestamp(file_name: &str, prefix: &str) -> Option<u128> {
    let fields = file_name.strip_prefix(prefix)?.strip_suffix(".json")?;
    let fields: Vec<&str> = fields.split('-').collect();
    if fields.len() == 3
        && fields[0].len() == 39
        && fields
            .iter()
            .all(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        fields[0].parse().ok()
    } else {
        None
    }
}

/// Loads the snapshot saved as `name`.
pub fn load(paths: &Paths, name: &str) -> anyhow::Result<Snapshot> {
    load_file(&snapshot_path(paths, name), name)
}

/// Loads the snapshot in `path`, which holds the layout saved as `name` (the name is only what an
/// error calls it): `<sessions_dir>/<name>.json`, or a copy of it in `session-snapshots/`.
pub fn load_file(path: &std::path::Path, name: &str) -> anyhow::Result<Snapshot> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let snap: Snapshot =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if !readable_by(snap.version, CURRENT_VERSION) {
        return Err(SnapshotError::NewerFormat {
            name: name.to_string(),
            found: snap.version,
            supported: CURRENT_VERSION,
        }
        .into());
    }
    Ok(snap)
}

/// A saved layout a plain restore can read: the file it is in, and the time its layout was taken
/// (`taken_unix`, read from the file itself; a copy's file name holds the time it was copied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub taken_unix: u64,
}

/// The layout a plain restore looks back to, chosen from what is saved and when the tmux server
/// started (`start`, unix seconds): the newest layout of the name taken strictly before `start`.
/// That is `default` (the file `<name>.json`) when it was taken before `start`, and otherwise the
/// newest of the `copies` that was. One layout only, never one further back: when the newest copy
/// from before `start` has every session open, no older one is looked at, and none taken at `start`
/// or later is chosen. `None` when none was taken before `start`. Of two copies taken in the same second the later one in `copies`, which is in the
/// order they were made, wins. A pure function of what it is given: the choice is by each layout's
/// own `taken_unix`, and never by a file's name or time.
pub fn newest_before(
    start: u64,
    default: Option<&Candidate>,
    copies: &[Candidate],
) -> Option<Candidate> {
    if let Some(default) = default.filter(|d| d.taken_unix < start) {
        return Some(default.clone());
    }
    let mut chosen: Option<&Candidate> = None;
    for copy in copies.iter().filter(|c| c.taken_unix < start) {
        if chosen.is_none_or(|best| copy.taken_unix >= best.taken_unix) {
            chosen = Some(copy);
        }
    }
    chosen.cloned()
}

/// [`newest_before`] over what is on disk: `<name>.json` and the copies of `name` in
/// `session-snapshots/`, each read for its own `taken_unix`. A file that does not parse, or that a
/// newer taw wrote, is not a layout to restore and is left out. A copy of another name is never
/// taken for one of `name` (see [`backup_timestamp`]).
pub fn look_back(paths: &Paths, name: &str, start: u64) -> Option<Candidate> {
    let taken = |path: PathBuf| {
        let raw = std::fs::read(&path).ok()?;
        let snap: Snapshot = serde_json::from_slice(&raw).ok()?;
        readable_by(snap.version, CURRENT_VERSION).then_some(Candidate {
            path,
            taken_unix: snap.taken_unix,
        })
    };
    let default = taken(snapshot_path(paths, name));
    let dir = paths.sessions_dir.join(SNAPSHOTS_DIR);
    let copies: Vec<Candidate> = backup_files(&dir, &format!("session-{name}-"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, path)| taken(path))
        .collect();
    newest_before(start, default.as_ref(), &copies)
}

/// Summary of one saved snapshot, for `taw session list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub name: String,
    pub path: PathBuf,
    pub taken_unix: u64,
    pub sessions: usize,
}

/// Every `<name>.json` regular file directly in the sessions directory, as `(name, path)`,
/// in directory order. Files that are not `<name>.json` (a leftover `.tmp`, the autosave
/// marker, the `session-backups/` directory) are not saved snapshots and never appear.
///
/// The one definition of "a saved snapshot" that [`list`] and [`delete`] share, so a name
/// `taw session list` shows is exactly a name `taw session delete` accepts.
fn snapshot_files(paths: &Paths) -> anyhow::Result<Vec<(String, PathBuf)>> {
    if !paths.sessions_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&paths.sessions_dir)
        .with_context(|| format!("reading {}", paths.sessions_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        files.push((name.to_string(), path));
    }
    Ok(files)
}

/// Every saved snapshot under `<sessions_dir>`, sorted by name (see [`snapshot_files`]).
pub fn list(paths: &Paths) -> anyhow::Result<Vec<SnapshotInfo>> {
    let mut out = Vec::new();
    for (name, path) in snapshot_files(paths)? {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let snap: Snapshot =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        out.push(SnapshotInfo {
            name,
            path,
            taken_unix: snap.taken_unix,
            sessions: snap.sessions.len(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// Follows herdr's src/session.rs. Differences: a saved session is one
// <name>.json here, not a per-name directory, so the exact-name check compares the sessions
// directory's file stems; "a different spelling exists" and "no such session" are both NotFound.
/// Deletes the snapshot saved as `name`, which has to be one of [`snapshot_files`]' names
/// *exactly* — spelling and case included, as `taw session list` shows it — rather than
/// whatever `<sessions_dir>/<name>.json` happens to resolve to. On a case-insensitive
/// filesystem (macOS's default) that path reaches a differently spelled snapshot
/// (`Default` finds `default.json`), and a name carrying `/` or `..` reaches outside the
/// sessions directory altogether. A name matching no snapshot is an `io::ErrorKind::NotFound`
/// error, which the CLI reports as `not_found`.
///
/// Follows herdr's `session.rs` `exact_session_dir_for_delete` (#3819): it too
/// reads the directory and compares entry names exactly instead of trusting a path built
/// from the name. herdr additionally tells "a different spelling exists" apart from "no
/// such session" with a distinct error; here both are `NotFound`, and [`case_variant`]
/// lets the CLI name the spelling that would have worked.
pub fn delete(paths: &Paths, name: &str) -> anyhow::Result<()> {
    let Some((_, path)) = snapshot_files(paths)?
        .into_iter()
        .find(|(saved, _)| saved == name)
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no saved session named '{name}'"),
        )
        .into());
    };
    // herdr copies before its `clear`: what taw never clears by itself, a delete is the nearest.
    preserve_snapshot_history(paths, name, None);
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// A saved snapshot whose name differs from `name` only in letter case — the one a
/// case-insensitive filesystem would have quietly resolved a wrong-cased name to, which the
/// CLI names when it refuses that spelling.
pub fn case_variant(paths: &Paths, name: &str) -> Option<String> {
    let lowered = name.to_lowercase();
    snapshot_files(paths)
        .ok()?
        .into_iter()
        .map(|(saved, _)| saved)
        .find(|saved| saved != name && saved.to_lowercase() == lowered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(dir: &std::path::Path) -> Paths {
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

    /// A pane listed with a claude that reported `conv-1` and the command that resumes it, the
    /// program in front of it (`command`), and its container mark and its profile mark as given.
    fn listed(command: &str, container: Option<&str>, sandbox: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: "%3".to_string(),
            session_id: "$1".to_string(),
            session_name: "alpha".to_string(),
            session_path: "/repo".to_string(),
            session_attached: 1,
            window_id: "@1".to_string(),
            window_index: 0,
            window_name: "editor".to_string(),
            window_active: true,
            window_layout: "abcd,80x24,0,0,0".to_string(),
            pane_index: 0,
            pane_active: true,
            pane_pid: 4242,
            pane_current_command: command.to_string(),
            pane_current_path: "/repo".to_string(),
            pane_title: String::new(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: crate::store::AgentRecord {
                kind: Some("claude".to_string()),
                session_id: Some("conv-1".to_string()),
                session_path: Some("/home/you/.claude/projects/x/conv-1.jsonl".to_string()),
                resume: Some(crate::agent_resume::ReportedResume {
                    source: "integration:claude".to_string(),
                    agent: "claude".to_string(),
                    argv: vec!["claude".into(), "--resume".into(), "conv-1".into()],
                }),
                container: container.map(str::to_string),
                sandbox: sandbox.map(str::to_string),
                ..crate::store::AgentRecord::default()
            },
        }
    }

    /// A pane whose agent runs in a container is saved with its session and the runtime beside it,
    /// whether taw started the container (both marks) or it was typed by hand (the container alone),
    /// and from the moment a container runtime is in front of the pane, before the watcher has read it
    /// and marked the pane: a restore resumes it only through the profile saved with it, and not at all
    /// with no profile (`restore::plan_of`). The profile mark alone (a first start still building
    /// its image, its shell script in front) and a plain agent are saved with no runtime. A runtime in
    /// front of a pane with nothing to resume is not saved.
    #[test]
    fn a_pane_whose_agent_runs_in_a_container_is_saved_with_its_session_and_the_runtime_beside_it()
    {
        for (command, container, sandbox, runtime) in [
            ("docker", Some("docker"), Some("default"), Some("docker")),
            ("podman", Some("podman"), None, Some("podman")),
            ("docker", None, None, Some("docker")),
            ("com.docker.cli", None, Some("default"), Some("docker")),
            ("bash", None, Some("default"), None),
            ("claude", None, None, None),
        ] {
            let saved = pane_snap(&listed(command, container, sandbox));
            let case = format!("{command} {container:?} {sandbox:?}");
            assert_eq!(saved.agent_kind.as_deref(), Some("claude"), "{case}");
            assert_eq!(saved.agent_session_id.as_deref(), Some("conv-1"), "{case}");
            assert!(saved.agent_session_path.is_some(), "{case}");
            assert_eq!(
                saved.agent_resume.map(|resume| resume.line()),
                Some("claude --resume conv-1".to_string()),
                "{case}"
            );
            assert_eq!(saved.agent_sandbox.as_deref(), sandbox, "{case}");
            assert_eq!(saved.agent_container.as_deref(), runtime, "{case}");
        }
        let mut nothing = listed("docker", None, None);
        nothing.agent = crate::store::AgentRecord::default();
        assert_eq!(pane_snap(&nothing).agent_container, None);
    }

    /// A restored sandbox still starting is saved with the conversation its restore resumed: the kept
    /// resume stands in for a session the pane's record does not hold, and for nothing else.
    #[test]
    fn a_restored_sandbox_still_starting_is_saved_with_the_session_it_resumes() {
        let base = sample().sessions[0].windows[0].panes[0].clone();
        let marked = PaneSnap {
            agent_sandbox: Some("default".to_string()),
            agent_kind: None,
            agent_session_id: None,
            agent_session_path: None,
            ..base
        };
        let kept = serde_json::to_string(&KeptResume {
            kind: "claude".to_string(),
            session_id: Some("4b1f2c".to_string()),
            ..KeptResume::default()
        })
        .unwrap();

        let saved = with_kept_resume(marked.clone(), Some(&kept));
        assert_eq!(
            (
                saved.agent_kind.as_deref(),
                saved.agent_session_id.as_deref()
            ),
            (Some("claude"), Some("4b1f2c"))
        );
        assert_eq!(saved.agent_sandbox.as_deref(), Some("default"));

        // The agent's own session wins once it reported one; a pane with no mark keeps nothing; what
        // is not a kept resume is nothing.
        let reported = PaneSnap {
            agent_kind: Some("claude".to_string()),
            agent_session_id: Some("new-one".to_string()),
            ..marked.clone()
        };
        assert_eq!(
            with_kept_resume(reported, Some(&kept))
                .agent_session_id
                .as_deref(),
            Some("new-one")
        );
        let unmarked = PaneSnap {
            agent_sandbox: None,
            ..marked.clone()
        };
        assert_eq!(
            with_kept_resume(unmarked, Some(&kept)).agent_session_id,
            None
        );
        assert_eq!(
            with_kept_resume(marked, Some("not json")).agent_session_id,
            None
        );
    }

    /// The kind comes with the session it is the kind of: a record that holds another kind and no
    /// session (an agent's word racing the restore) is saved with the kept resume's kind beside the
    /// kept session, never one agent's kind with another's session.
    #[test]
    fn a_kept_session_is_saved_with_the_kept_kind_whatever_kind_the_record_holds() {
        let base = sample().sessions[0].windows[0].panes[0].clone();
        let marked = PaneSnap {
            agent_sandbox: Some("default".to_string()),
            agent_kind: Some("codex".to_string()),
            agent_session_id: None,
            agent_session_path: None,
            ..base
        };
        let kept = serde_json::to_string(&KeptResume {
            kind: "claude".to_string(),
            session_id: Some("4b1f2c".to_string()),
            ..KeptResume::default()
        })
        .unwrap();

        let saved = with_kept_resume(marked, Some(&kept));

        assert_eq!(
            (
                saved.agent_kind.as_deref(),
                saved.agent_session_id.as_deref()
            ),
            (Some("claude"), Some("4b1f2c"))
        );
    }

    /// The reported command comes with the session it resumes, as the kind does: a record that holds
    /// a command of its own and no session (a plain report landing beside the mark) is saved with the
    /// kept resume's command, or none, never its own beside the kept session.
    #[test]
    fn a_kept_session_is_saved_with_the_kept_command_whatever_command_the_record_holds() {
        let base = sample().sessions[0].windows[0].panes[0].clone();
        let own = crate::agent_resume::ReportedResume {
            source: "integration:codex".to_string(),
            agent: "codex".to_string(),
            argv: ["codex", "resume", "--last"].map(String::from).to_vec(),
        };
        let marked = PaneSnap {
            agent_sandbox: Some("default".to_string()),
            agent_kind: Some("codex".to_string()),
            agent_session_id: None,
            agent_session_path: None,
            agent_resume: Some(own),
            ..base
        };
        let kept_with = |agent_resume| {
            serde_json::to_string(&KeptResume {
                kind: "claude".to_string(),
                session_id: Some("4b1f2c".to_string()),
                agent_resume,
                ..KeptResume::default()
            })
            .unwrap()
        };

        let saved = with_kept_resume(marked.clone(), Some(&kept_with(None)));
        assert_eq!(saved.agent_session_id.as_deref(), Some("4b1f2c"));
        assert_eq!(saved.agent_resume, None);

        let kept = crate::agent_resume::ReportedResume {
            source: "integration:claude".to_string(),
            agent: "claude".to_string(),
            argv: ["claude", "--resume", "4b1f2c"].map(String::from).to_vec(),
        };
        let saved = with_kept_resume(marked, Some(&kept_with(Some(kept.clone()))));
        assert_eq!(saved.agent_resume, Some(kept));
    }

    fn sample() -> Snapshot {
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
                        agent_kind: Some("claude".to_string()),
                        agent_session_id: Some("abc".to_string()),
                        agent_session_path: Some("/repo".to_string()),
                        agent_name: Some("reviewer".to_string()),
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

    /// herdr's `layout_fingerprint_survives_json_round_trip`: a snapshot saved and loaded back has
    /// the fingerprint it had, with a pane's reported resume command present and absent, and the
    /// time it was taken is no part of it while any other change is.
    #[test]
    fn layout_fingerprint_survives_json_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let mut with = sample();
        with.sessions[0].windows[0].panes[0].agent_resume =
            Some(crate::agent_resume::ReportedResume {
                source: "claude".to_string(),
                agent: "claude".to_string(),
                argv: vec![
                    "claude".to_string(),
                    "--resume".to_string(),
                    "abc".to_string(),
                ],
            });
        for snap in [sample(), with] {
            let fingerprint = layout_fingerprint(&snap).unwrap();
            assert_eq!(fingerprint.len(), 64, "{fingerprint}");
            save(&paths, "round", &snap).unwrap();
            assert_eq!(
                layout_fingerprint(&load(&paths, "round").unwrap()).as_deref(),
                Some(fingerprint.as_str())
            );
            let later = Snapshot {
                taken_unix: snap.taken_unix + 5,
                ..snap.clone()
            };
            assert_eq!(layout_fingerprint(&later).unwrap(), fingerprint);
            let mut moved = snap.clone();
            moved.sessions[0].windows[0].layout = "efgh,80x24,0,0,1".to_string();
            assert_ne!(layout_fingerprint(&moved).unwrap(), fingerprint);
        }
        assert_ne!(
            layout_fingerprint(&sample()),
            layout_fingerprint(&Snapshot {
                skipped_panes: 1,
                ..sample()
            })
        );
        // The format version counts: the same layout saved under another format is another one.
        assert_ne!(
            layout_fingerprint(&sample()),
            layout_fingerprint(&Snapshot {
                version: sample().version + 1,
                ..sample()
            })
        );
    }

    /// A snapshot written before panes kept a reported resume command has no `agent_resume`, and loads
    /// with none; one that has it reads it back; and a pane with none writes no field for it.
    #[test]
    fn a_reported_resume_command_is_saved_with_its_pane_and_old_snapshots_load_without_one() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let mut snap = sample();
        let written = serde_json::to_string(&snap).unwrap();
        assert!(!written.contains("agent_resume"), "{written}");

        // An old snapshot, as one was written before.
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        std::fs::write(paths.sessions_dir.join("old.json"), &written).unwrap();
        assert_eq!(load(&paths, "old").unwrap(), snap);

        let resume = crate::agent_resume::ReportedResume {
            source: "prime-agent".to_string(),
            agent: "prime-agent".to_string(),
            argv: ["prime-agent", "--resume", "a"].map(String::from).to_vec(),
        };
        snap.sessions[0].windows[0].panes[0].agent_resume = Some(resume.clone());
        save(&paths, "new", &snap).unwrap();
        let raw = std::fs::read_to_string(paths.sessions_dir.join("new.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            json["sessions"][0]["windows"][0]["panes"][0]["agent_resume"],
            serde_json::json!({
                "source": "prime-agent",
                "agent": "prime-agent",
                "argv": ["prime-agent", "--resume", "a"],
            })
        );
        let loaded = load(&paths, "new").unwrap();
        assert_eq!(
            loaded.sessions[0].windows[0].panes[0].agent_resume,
            Some(resume)
        );
    }

    #[test]
    fn a_pane_keeps_the_profile_its_agent_was_started_through_and_the_runtime_it_ran_under() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut snap = sample();
        let pane = &mut snap.sessions[0].windows[0].panes[0];
        pane.agent_sandbox = Some("default".to_string());
        pane.agent_container = Some("docker".to_string());

        save(&paths, "default", &snap).unwrap();
        let back = load(&paths, "default").unwrap();

        assert_eq!(
            back.sessions[0].windows[0].panes[0]
                .agent_sandbox
                .as_deref(),
            Some("default")
        );
        assert_eq!(
            back.sessions[0].windows[0].panes[0]
                .agent_container
                .as_deref(),
            Some("docker")
        );
    }

    /// A save reads the profile and the runtime off the pane's marks, and a pane with neither writes
    /// no field for them: a plain pane's JSON stays as it was before format 2.
    #[test]
    fn a_save_takes_the_profile_and_the_runtime_from_the_pane_and_writes_none_for_a_plain_one() {
        let saved = pane_snap(&listed("bash", Some("docker"), Some("box")));
        assert_eq!(
            (
                saved.agent_sandbox.as_deref(),
                saved.agent_container.as_deref()
            ),
            (Some("box"), Some("docker"))
        );
        let written = serde_json::to_string(&sample()).unwrap();
        assert!(!written.contains("agent_sandbox"), "{written}");
        assert!(!written.contains("agent_container"), "{written}");
    }

    /// The previous release's reader knows formats up to 1: it refuses this taw's snapshots, rather
    /// than resuming a sandboxed agent on this machine.
    #[test]
    fn snapshots_are_format_2_which_a_reader_of_format_1_refuses() {
        assert_eq!(CURRENT_VERSION, 2);
        assert!(!readable_by(2, 1));
        assert!(readable_by(1, 2) && readable_by(2, 2));
    }

    #[test]
    fn a_format_1_snapshot_still_loads_with_no_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let old = serde_json::json!({
            "version": 1, "taken_unix": 0,
            "sessions": [{"name": "a", "path": "/a", "active_window_index": 0, "windows": [{"index": 0, "name": "w", "layout": "l", "active": true,
                "panes": [{"index": 0, "cwd": "/a", "active": true, "agent_kind": "claude", "agent_session_id": "x", "agent_session_path": null, "agent_name": null, "agent_kind_hint": null}]}]}],
        });
        std::fs::write(paths.sessions_dir.join("old.json"), old.to_string()).unwrap();

        let snap = load(&paths, "old").unwrap();

        assert_eq!(snap.sessions[0].windows[0].panes[0].agent_sandbox, None);
        assert_eq!(snap.sessions[0].windows[0].panes[0].agent_container, None);
        assert_eq!(
            snap.sessions[0].windows[0].panes[0]
                .agent_session_id
                .as_deref(),
            Some("x")
        );
    }

    /// A save keeps the snapshot it replaces in its history (`session-snapshots/`, 48 per name, the
    /// first copy of a name at once): one of format 2 is copied as one of format 1 is, its profile
    /// with it, and is not taken for a newer taw's.
    #[test]
    fn the_history_keeps_a_format_2_snapshot_as_it_keeps_one_of_format_1() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut first = sample();
        first.version = 2;
        first.sessions[0].windows[0].panes[0].agent_sandbox = Some("default".to_string());
        save(&paths, "default", &first).unwrap();
        let mut second = sample();
        second.sessions[0].windows[0].layout = "efgh,120x40,0,0,0".to_string();

        save(&paths, "default", &second).unwrap();

        let history = paths.sessions_dir.join("session-snapshots");
        let copies: Vec<PathBuf> = std::fs::read_dir(&history)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&copies[0]).unwrap()).unwrap();
        assert_eq!(kept["version"], 2);
        assert_eq!(
            kept["sessions"][0]["windows"][0]["panes"][0]["agent_sandbox"],
            "default"
        );
    }

    #[test]
    fn snapshot_round_trips_through_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());

        let snap = sample();
        let path = save(&paths, "default", &snap).unwrap();
        assert_eq!(path, paths.sessions_dir.join("default.json"));
        assert!(!paths.sessions_dir.join("default.json.tmp").exists());

        let loaded = load(&paths, "default").unwrap();
        assert_eq!(loaded, snap);
    }

    #[test]
    fn list_reports_every_saved_snapshot_and_delete_removes_one() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());

        save(&paths, "default", &sample()).unwrap();
        save(&paths, "before-deploy", &sample()).unwrap();

        let infos = list(&paths).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "before-deploy");
        assert_eq!(infos[1].name, "default");
        assert_eq!(infos[1].sessions, 1);
        assert_eq!(infos[1].taken_unix, 1_700_000_000);

        delete(&paths, "default").unwrap();
        let infos = list(&paths).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "before-deploy");
    }

    #[test]
    fn list_on_a_missing_sessions_dir_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        assert!(list(&paths).unwrap().is_empty());
    }

    // -------------------------------------------------------------------------------
    // Deleting a saved session needs its exact name — herdr v0.9.1 (#3819, #3220): on a
    // case-insensitive filesystem (macOS's default) `<sessions>/Default.json` *is*
    // `default.json`, so a path built from the name alone deleted a snapshot the name did
    // not match.
    // -------------------------------------------------------------------------------

    #[test]
    fn delete_needs_the_saved_names_exact_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();

        let err = delete(&paths, "Default").unwrap_err();

        assert!(is_not_found(&err), "{err:#}");
        assert_eq!(
            load(&paths, "default").unwrap(),
            sample(),
            "a differently-spelled name must not delete the snapshot"
        );

        delete(&paths, "default").unwrap();
        assert!(list(&paths).unwrap().is_empty());
    }

    #[test]
    fn delete_cannot_reach_a_file_outside_the_sessions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        let outside = paths.state_dir.join("outside.json");
        std::fs::write(&outside, b"not a snapshot").unwrap();

        let err = delete(&paths, "../outside").unwrap_err();

        assert!(is_not_found(&err), "{err:#}");
        assert!(outside.exists(), "a name is never a path");
    }

    #[test]
    fn delete_removes_a_snapshot_taw_can_no_longer_read() {
        // `list` parses every file, so it cannot be what finds the one to delete: a damaged
        // or newer-format snapshot has to stay deletable by its name.
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let broken = snapshot_path(&paths, "broken");
        std::fs::write(&broken, b"not valid json").unwrap();

        delete(&paths, "broken").unwrap();

        assert!(!broken.exists());
    }

    #[test]
    fn delete_on_a_missing_sessions_dir_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());

        let err = delete(&paths, "default").unwrap_err();

        assert!(is_not_found(&err), "{err:#}");
    }

    #[test]
    fn case_variant_names_the_spelling_that_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        save(&paths, "before-deploy", &sample()).unwrap();

        assert_eq!(case_variant(&paths, "Default").as_deref(), Some("default"));
        assert_eq!(
            case_variant(&paths, "BEFORE-DEPLOY").as_deref(),
            Some("before-deploy")
        );
        assert_eq!(case_variant(&paths, "default"), None, "an exact name");
        assert_eq!(case_variant(&paths, "other"), None, "no such snapshot");
    }

    // -------------------------------------------------------------------------------
    // Preserving an unreadable saved session before it is replaced — ported from
    // herdr's `persist/writer.rs` tests (v0.9.1, #4125), adapted to this crate's model:
    // taw checks the existing file's own readability on every save, rather than
    // herdr's `protect_unloaded` flag carried across a long-lived process's saves.
    // -------------------------------------------------------------------------------

    fn backups_of(paths: &Paths, name: &str) -> Vec<Vec<u8>> {
        let dir = paths.sessions_dir.join("session-backups");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut files: Vec<_> = entries
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("session-{name}-")))
            })
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|p| std::fs::read(&p).unwrap())
            .collect()
    }

    #[test]
    fn save_makes_no_backup_when_the_existing_file_is_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();

        save(&paths, "default", &sample()).unwrap();

        assert!(
            backups_of(&paths, "default").is_empty(),
            "a healthy existing file must not be backed up"
        );
    }

    #[test]
    fn save_preserves_an_unreadable_existing_file_before_replacing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let path = snapshot_path(&paths, "default");
        std::fs::write(&path, b"not valid json").unwrap();

        save(&paths, "default", &sample()).unwrap();

        assert_eq!(load(&paths, "default").unwrap(), sample());
        assert_eq!(
            backups_of(&paths, "default"),
            vec![b"not valid json".to_vec()]
        );
    }

    #[test]
    fn save_skips_entirely_when_preservation_fails_and_leaves_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let path = snapshot_path(&paths, "default");
        std::fs::write(&path, b"not valid json").unwrap();
        // Block the backup directory: a file sits where session-backups/ needs to be a
        // directory, so create_dir_all fails.
        std::fs::write(paths.sessions_dir.join("session-backups"), b"in the way").unwrap();

        let result = save(&paths, "default", &sample());

        assert!(result.is_err(), "preservation failure must fail the save");
        assert_eq!(std::fs::read(&path).unwrap(), b"not valid json");
    }

    #[test]
    fn save_keeps_only_the_three_newest_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let path = snapshot_path(&paths, "default");
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();

        for i in 0..5u8 {
            std::fs::write(&path, [i]).unwrap();
            save(&paths, "default", &sample()).unwrap();
        }

        assert_eq!(
            backups_of(&paths, "default"),
            vec![vec![2], vec![3], vec![4]]
        );

        // A further healthy save/save cycle does not rotate them again.
        save(&paths, "default", &sample()).unwrap();
        assert_eq!(
            backups_of(&paths, "default"),
            vec![vec![2], vec![3], vec![4]]
        );
    }

    /// Every backup file in the directory, by content, whatever snapshot name it belongs to.
    fn all_backups(paths: &Paths) -> Vec<Vec<u8>> {
        let dir = paths.sessions_dir.join("session-backups");
        let mut contents: Vec<Vec<u8>> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| std::fs::read(e.unwrap().path()).unwrap())
            .collect();
        contents.sort();
        contents
    }

    /// `proj-2024` starts with `proj-`, so a backup of it is named `session-proj-2024-...`,
    /// which is also how a backup of `proj` begins. Counting it as one of `proj`'s (with
    /// `2024` read as its timestamp, which makes it the oldest) meant saving `proj` pruned
    /// the only preserved copies of another snapshot, in the feature built to keep them.
    #[test]
    fn one_names_backups_are_never_pruned_as_another_names() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();

        for (name, tag) in [("proj-2024", b'q'), ("proj", b'p')] {
            for i in 0..3u8 {
                std::fs::write(snapshot_path(&paths, name), [tag, i]).unwrap();
                save(&paths, name, &sample()).unwrap();
            }
        }

        assert_eq!(
            all_backups(&paths),
            vec![
                vec![b'p', 0],
                vec![b'p', 1],
                vec![b'p', 2],
                vec![b'q', 0],
                vec![b'q', 1],
                vec![b'q', 2],
            ],
            "each name keeps its own three"
        );
    }

    /// Follows herdr's `recovery_timestamp`: a backup is recognised by its whole name,
    /// `<39 digits>-<digits>-<digits>.json` after the prefix, not by whatever follows the
    /// first dash.
    #[test]
    fn a_backup_is_recognised_only_by_the_exact_shape_it_is_written_in() {
        let stamp = format!("{:039}", 1_700_000_000_000_000_000u128);
        let ok = format!("session-proj-{stamp}-4321-0.json");
        assert_eq!(
            backup_timestamp(&ok, "session-proj-"),
            Some(1_700_000_000_000_000_000)
        );

        let other_name = format!("session-proj-2024-{stamp}-4321-0.json");
        for (why, name) in [
            ("another snapshot's backup", other_name),
            (
                "a timestamp of the wrong width",
                "session-proj-2024-1-0.json".to_string(),
            ),
            (
                "a missing sequence",
                format!("session-proj-{stamp}-4321.json"),
            ),
            (
                "an extra field",
                format!("session-proj-{stamp}-4321-0-1.json"),
            ),
            (
                "a non-digit pid",
                format!("session-proj-{stamp}-pid-0.json"),
            ),
            ("an empty field", format!("session-proj-{stamp}--0.json")),
            (
                "another extension",
                format!("session-proj-{stamp}-4321-0.tmp"),
            ),
            (
                "another prefix",
                format!("session-other-{stamp}-4321-0.json"),
            ),
        ] {
            assert_eq!(backup_timestamp(&name, "session-proj-"), None, "{why}");
        }
    }

    /// `Snapshot` ignores fields it does not know, so a newer taw's file that only adds
    /// some still parses, and must not be taken for a healthy one and overwritten with no
    /// backup: the README promises the copy when a newer taw wrote it.
    #[test]
    fn save_preserves_a_snapshot_a_newer_taw_wrote_even_when_it_still_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let newer = serde_json::to_vec(&serde_json::json!({
            "version": CURRENT_VERSION + 1,
            "taken_unix": 1_700_000_000u64,
            "sessions": [],
            "a_field_taw_does_not_know": {"x": 1},
        }))
        .unwrap();
        assert!(
            serde_json::from_slice::<Snapshot>(&newer).is_ok(),
            "the premise: this parses as a Snapshot"
        );
        std::fs::write(snapshot_path(&paths, "default"), &newer).unwrap();

        save(&paths, "default", &sample()).unwrap();

        assert_eq!(backups_of(&paths, "default"), vec![newer]);
        assert_eq!(load(&paths, "default").unwrap(), sample());
    }

    #[test]
    fn a_snapshot_of_this_version_with_extra_fields_is_healthy_and_not_backed_up() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let same = serde_json::to_vec(&serde_json::json!({
            "version": CURRENT_VERSION,
            "taken_unix": 1_700_000_000u64,
            "sessions": [],
            "a_field_added_by_a_patch_release": true,
        }))
        .unwrap();
        std::fs::write(snapshot_path(&paths, "default"), &same).unwrap();

        save(&paths, "default", &sample()).unwrap();

        assert!(backups_of(&paths, "default").is_empty());
    }

    /// The same rule as herdr's (`unsupported_version`): a saved session from a newer
    /// version is not restored by an older one, which would rebuild it without whatever
    /// the newer format added. The file is left exactly as it is.
    #[test]
    fn load_refuses_a_snapshot_a_newer_taw_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let newer = serde_json::to_vec(&serde_json::json!({
            "version": CURRENT_VERSION + 1,
            "taken_unix": 1u64,
            "sessions": [],
        }))
        .unwrap();
        std::fs::write(snapshot_path(&paths, "default"), &newer).unwrap();

        let err = load(&paths, "default").unwrap_err();

        assert!(
            matches!(
                err.downcast_ref::<SnapshotError>(),
                Some(SnapshotError::NewerFormat { found, .. }) if *found == CURRENT_VERSION + 1
            ),
            "{err:#}"
        );
        let text = err.to_string();
        assert!(
            text.contains("'default'") && text.contains("newer taw") && text.contains("update taw"),
            "{text}"
        );
        assert_eq!(
            std::fs::read(snapshot_path(&paths, "default")).unwrap(),
            newer
        );
    }

    /// Two saves of one name (several `session-closed` hooks whose five-second waits end
    /// together) staged at the same temporary path and could rename each other's file away.
    #[test]
    fn every_save_stages_under_a_name_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());

        let first = staging_path(&paths, "default");
        let second = staging_path(&paths, "default");

        assert_ne!(first, second);
        for staged in [&first, &second] {
            assert_eq!(staged.parent(), Some(paths.sessions_dir.as_path()));
            assert!(
                staged.extension().is_some_and(|e| e == "tmp"),
                "a staged file must never look like a saved snapshot: {}",
                staged.display()
            );
        }
    }

    #[test]
    fn concurrent_saves_of_one_name_all_succeed_and_leave_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let snap = sample();

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        save(&paths, "default", &snap).unwrap();
                    }
                });
            }
        });

        assert_eq!(load(&paths, "default").unwrap(), snap);
        let left: Vec<_> = std::fs::read_dir(&paths.sessions_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != SNAPSHOTS_DIR)
            .collect();
        assert_eq!(
            left,
            vec!["default.json".to_string()],
            "no staging leftovers"
        );
        let pending: Vec<_> = std::fs::read_dir(paths.sessions_dir.join(SNAPSHOTS_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".json"))
            .collect();
        assert!(pending.is_empty(), "no pending copy left: {pending:?}");
    }

    /// A snapshot whose JSON runs far past a one-block file-size limit.
    fn large_snapshot() -> Snapshot {
        let mut snap = sample();
        let template = snap.sessions[0].clone();
        snap.sessions = (0..40)
            .map(|i| SessionSnap {
                name: format!("session-{i}"),
                ..template.clone()
            })
            .collect();
        snap
    }

    /// A write that dies partway (here the file-size limit, `EFBIG`, standing in for a full
    /// disk) leaves behind the staging file it had started. Nothing else would ever remove
    /// it: it is not a saved snapshot, and every save stages under a name of its own. `save`
    /// takes it away, as it does when the rename fails, and the snapshot that was already
    /// saved is untouched. Runs as a child of a shell that sets the limit and ignores the
    /// signal it raises, as the config writer's own partial-write test does.
    #[cfg(unix)]
    #[test]
    fn save_removes_its_staging_file_when_the_write_fails() {
        const CHILD: &str = "TAW_SNAPSHOT_PARTIAL_WRITE_TEST";
        if let Some(dir) = std::env::var_os(CHILD) {
            let paths = test_paths(std::path::Path::new(&dir));
            let err = save(&paths, "default", &large_snapshot()).unwrap_err();
            let io = err
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                .unwrap_or_else(|| panic!("no io error behind {err:#}"));
            assert_eq!(io.kind(), std::io::ErrorKind::FileTooLarge, "{err:#}");
            println!("partial-write path executed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();

        let output = std::process::Command::new("bash")
            .args(["-c", "trap '' XFSZ; ulimit -f 1; exec \"$@\"", "taw-test"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::snapshot::tests::save_removes_its_staging_file_when_the_write_fails",
                "--nocapture",
            ])
            .env(CHILD, tmp.path())
            .output()
            .unwrap();

        assert!(output.status.success(), "child failed: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("partial-write path executed"));
        assert_eq!(
            load(&paths, "default").unwrap(),
            sample(),
            "the snapshot that was saved must be untouched"
        );
        let left: Vec<_> = std::fs::read_dir(&paths.sessions_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != SNAPSHOTS_DIR)
            .collect();
        assert_eq!(
            left,
            vec!["default.json".to_string()],
            "a failed write must not leave its staging file behind"
        );
    }

    #[test]
    fn backup_order_survives_a_clock_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let path = snapshot_path(&paths, "default");
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();

        std::fs::write(&path, b"first").unwrap();
        preserve_unreadable_existing(&paths, "default", 1_000_000).unwrap();

        // The wall clock moves backwards for the second preservation.
        std::fs::write(&path, b"second").unwrap();
        preserve_unreadable_existing(&paths, "default", 500).unwrap();

        assert_eq!(
            backups_of(&paths, "default"),
            vec![b"first".to_vec(), b"second".to_vec()],
            "creation order must survive even though the second clock reading was earlier"
        );
    }

    // -------------------------------------------------------------------------------
    // The copies of a name kept in session-snapshots/ (herdr keeps 48 snapshots). The first four
    // are herdr's own tests (src/persist/writer.rs@v0.9.3), each save being its own call here
    // and herdr's `clear` being `delete`.
    // -------------------------------------------------------------------------------

    use std::fs::{File, FileTimes};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn copies_of(paths: &Paths, name: &str) -> Vec<(u128, PathBuf)> {
        backup_files(
            &paths.sessions_dir.join(SNAPSHOTS_DIR),
            &format!("session-{name}-"),
        )
        .unwrap_or_default()
    }

    fn set_modified(path: &std::path::Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap();
    }

    /// A copy whose prune fails (every older copy undeletable: all 48 are made immutable, since
    /// pruning passes over one it cannot delete and removes the next) is removed again, so the
    /// folder does not grow by one copy every cadence for good, and the save goes ahead.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_copy_whose_prune_fails_is_removed_again_and_the_save_goes_ahead() {
        use std::os::unix::ffi::OsStrExt;
        struct Unlock(Vec<PathBuf>);
        impl Drop for Unlock {
            fn drop(&mut self) {
                for path in &self.0 {
                    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    unsafe { libc::chflags(c.as_ptr(), 0) };
                }
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let dir = paths.sessions_dir.join(SNAPSHOTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let old = |i: usize| dir.join(format!("session-default-{i:039}-1-0.json"));
        let mut unlock = Unlock(Vec::new());
        for i in 0..48 {
            std::fs::write(old(i), b"old").unwrap();
            set_modified(&old(i), UNIX_EPOCH);
            let c = std::ffi::CString::new(old(i).as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chflags(c.as_ptr(), libc::UF_IMMUTABLE) }, 0);
            unlock.0.push(old(i));
        }
        save(&paths, "default", &sample()).unwrap();
        assert_eq!(load(&paths, "default").unwrap(), sample());
        let files = copies_of(&paths, "default");
        assert_eq!(files.len(), 48, "{files:?}");
        assert!(
            files
                .iter()
                .all(|(_, p)| std::fs::read(p).unwrap() == b"old")
        );
        let others: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| !n.to_string_lossy().ends_with(".json"))
            .collect();
        assert!(others.is_empty(), "{others:?}");
    }

    /// `sample()` with its window named `label`: another layout, not another time.
    fn labelled(label: &str) -> Snapshot {
        let mut snap = sample();
        snap.sessions[0].windows[0].name = label.to_string();
        snap
    }

    #[test]
    fn snapshot_survives_exit_bursts_clears_and_writer_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        let files = copies_of(&paths, "default");
        assert_eq!(files.len(), 1, "the first save is copied at once");
        let kept = std::fs::read(&files[0].1).unwrap();
        for i in 0..100 {
            save(&paths, "default", &labelled(&format!("remaining pane {i}"))).unwrap();
        }
        assert_eq!(copies_of(&paths, "default"), files, "a burst adds none");
        delete(&paths, "default").unwrap();
        assert!(!snapshot_path(&paths, "default").exists());
        assert_eq!(copies_of(&paths, "default"), files);
        assert_eq!(std::fs::read(&files[0].1).unwrap(), kept);
        assert!(!paths.sessions_dir.join(BACKUPS_DIR).exists());
    }

    #[test]
    fn snapshot_history_is_bounded_and_does_not_rotate_identical_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let dir = paths.sessions_dir.join(SNAPSHOTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let manual = dir.join("my-layout.json");
        std::fs::write(&manual, b"manual").unwrap();
        // Another name, one whose own name starts with this one's: neither is this name's copy.
        let others: Vec<PathBuf> = (0..50)
            .map(|i| dir.join(format!("session-default-2024-{i:039}-1-0.json")))
            .collect();
        for other in &others {
            std::fs::write(other, b"another name").unwrap();
        }
        let old = |i: usize| dir.join(format!("session-default-{i:039}-1-0.json"));
        for i in 0..48 {
            std::fs::write(old(i), b"old snapshot").unwrap();
        }
        for path in others.iter().cloned().chain((0..48).map(old)) {
            set_modified(&path, UNIX_EPOCH);
        }

        save(&paths, "default", &sample()).unwrap();

        assert_eq!(copies_of(&paths, "default").len(), 48);
        assert!(!old(0).exists(), "the oldest copy is pruned");
        assert!(old(1).exists());
        assert!(
            manual.exists(),
            "a file that is not named as a copy is never touched"
        );
        assert!(
            others.iter().all(|other| other.exists()),
            "another name's copies stay"
        );

        // An equal layout written with other key order and whitespace is not copied again, and
        // nor is the same layout taken at another time.
        for (_, path) in copies_of(&paths, "default") {
            std::fs::remove_file(path).unwrap();
        }
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(snapshot_path(&paths, "default")).unwrap())
                .unwrap();
        let reversed: serde_json::Map<String, serde_json::Value> = saved
            .as_object()
            .unwrap()
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let equivalent = serde_json::to_vec(&reversed).unwrap();
        assert_ne!(
            std::fs::read(snapshot_path(&paths, "default")).unwrap(),
            equivalent
        );
        std::fs::write(old(1), &equivalent).unwrap();
        set_modified(&old(1), UNIX_EPOCH);
        let later = Snapshot {
            taken_unix: sample().taken_unix + 60,
            ..sample()
        };
        save(&paths, "default", &later).unwrap();
        let files = copies_of(&paths, "default");
        assert_eq!(
            files,
            vec![(1, old(1))],
            "the same layout is not copied twice"
        );
        assert_eq!(std::fs::read(old(1)).unwrap(), equivalent);

        // A changed layout is, so the check above is not a copy step that never runs.
        save(&paths, "default", &labelled("changed")).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 2);
    }

    #[test]
    fn snapshot_cadence_recovers_after_clock_rollback_and_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        let file = copies_of(&paths, "default").pop().unwrap().1;
        set_modified(&file, SystemTime::now() + Duration::from_secs(86400));

        save(&paths, "default", &labelled("after clock rollback")).unwrap();
        assert_eq!(
            copies_of(&paths, "default").len(),
            2,
            "a newest copy dated in the future does not block"
        );

        save(&paths, "default", &labelled("after restart")).unwrap();
        assert_eq!(
            copies_of(&paths, "default").len(),
            2,
            "the new copy's time restores the cadence"
        );
    }

    #[test]
    fn snapshot_failure_does_not_block_primary_save_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let blocked = paths.sessions_dir.join(SNAPSHOTS_DIR);
        std::fs::write(&blocked, b"blocked").unwrap();

        save(&paths, "default", &sample()).unwrap();
        save(&paths, "default", &labelled("again")).unwrap();
        assert_eq!(load(&paths, "default").unwrap(), labelled("again"));
        delete(&paths, "default").unwrap();

        assert!(!snapshot_path(&paths, "default").exists());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"blocked");
    }

    #[test]
    fn pruning_continues_past_an_undeletable_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let locked = tmp.path().join("undeletable");
        std::fs::create_dir(&locked).unwrap();
        let removable = tmp.path().join("removable");
        std::fs::write(&removable, b"old").unwrap();

        assert!(prune_copies(&[(1, locked.clone()), (2, removable.clone())], 2).is_ok());
        assert!(locked.exists());
        assert!(!removable.exists());
        assert!(prune_copies(&[(1, locked)], 1).is_err());
    }

    /// A file that does not parse, one a newer taw wrote and one with no session are not copied,
    /// each shown beside the same call copying a good file.
    #[test]
    fn a_file_that_does_not_parse_is_newer_or_holds_no_session_is_not_copied() {
        let newer = Snapshot {
            version: CURRENT_VERSION + 1,
            ..sample()
        };
        let empty = Snapshot {
            sessions: Vec::new(),
            ..sample()
        };
        let cases: [(&str, Vec<u8>); 3] = [
            ("garbage", b"not json".to_vec()),
            ("newer", serde_json::to_vec(&newer).unwrap()),
            ("empty", serde_json::to_vec(&empty).unwrap()),
        ];
        for (what, bytes) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let paths = test_paths(tmp.path());
            std::fs::create_dir_all(&paths.sessions_dir).unwrap();
            let path = snapshot_path(&paths, "default");
            std::fs::write(&path, &bytes).unwrap();
            copy_into_history(&paths, "default", 5, None).unwrap();
            assert!(copies_of(&paths, "default").is_empty(), "{what} was copied");
            std::fs::write(&path, serde_json::to_vec(&sample()).unwrap()).unwrap();
            copy_into_history(&paths, "default", 5, None).unwrap();
            assert_eq!(
                copies_of(&paths, "default").len(),
                1,
                "{what}: a good file is"
            );
        }
    }

    /// A copy under 15 minutes old blocks the next; the same call copies once it is older.
    #[test]
    fn a_copy_younger_than_fifteen_minutes_blocks_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let path = snapshot_path(&paths, "default");
        std::fs::write(&path, serde_json::to_vec(&sample()).unwrap()).unwrap();
        copy_into_history(&paths, "default", 5, None).unwrap();
        let first = copies_of(&paths, "default").pop().unwrap().1;
        std::fs::write(&path, serde_json::to_vec(&labelled("two")).unwrap()).unwrap();

        set_modified(&first, SystemTime::now() - Duration::from_secs(14 * 60));
        copy_into_history(&paths, "default", 5, None).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 1);

        set_modified(&first, SystemTime::now() - Duration::from_secs(16 * 60));
        copy_into_history(&paths, "default", 5, None).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 2);
    }

    /// The copy rule as a pure function: a layout from before the server started is copied however
    /// new the newest copy is, one from after it waits out the 15 minutes, a layout the newest copy
    /// has is never copied twice, and with no server start known the rule is the old one.
    #[test]
    fn a_copy_skips_the_wait_only_for_a_layout_from_before_the_server_started() {
        let young = Some(Duration::from_secs(60));
        let old = Some(Duration::from_secs(16 * 60));
        // Taken at 1000, the server started at 1001: before, strictly.
        assert!(copy_is_due(young, 1000, Some(1001), false));
        // Taken in the very second it started: not before.
        assert!(!copy_is_due(young, 1001, Some(1001), false));
        assert!(!copy_is_due(young, 1002, Some(1001), false));
        // No server to ask: today's rule.
        assert!(!copy_is_due(young, 1000, None, false));
        assert!(copy_is_due(old, 1000, None, false));
        assert!(copy_is_due(None, 1000, None, false));
        // After the server started, the wait counts as it did.
        assert!(!copy_is_due(young, 2000, Some(1001), false));
        assert!(copy_is_due(old, 2000, Some(1001), false));
        assert!(copy_is_due(None, 2000, Some(1001), false));
        // The same layout is never copied twice, whatever else says.
        assert!(!copy_is_due(young, 1000, Some(1001), true));
        assert!(!copy_is_due(old, 1000, Some(1001), true));
        assert!(!copy_is_due(None, 1000, None, true));
    }

    /// The rule as a save applies it: with the newest copy a minute old, the layout on disk was taken
    /// before the server started and so is copied; the same file is not copied when the server
    /// started before it was taken, when no start is known, or when the copy already has its layout.
    #[test]
    fn a_save_on_a_new_server_copies_the_layout_it_replaces_whatever_the_newest_copys_age() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let path = snapshot_path(&paths, "default");
        std::fs::write(&path, serde_json::to_vec(&sample()).unwrap()).unwrap();
        copy_into_history(&paths, "default", 5, None).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 1);

        // A newer layout, taken at 1_700_000_000 + 60 and not copied: the copy is a minute old.
        let two = Snapshot {
            taken_unix: 1_700_000_060,
            ..labelled("two")
        };
        std::fs::write(&path, serde_json::to_vec(&two).unwrap()).unwrap();
        let start = 1_700_000_100;
        copy_into_history(&paths, "default", 6, None).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 1, "no start known");
        copy_into_history(&paths, "default", 6, Some(1_700_000_060)).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 1, "taken as it started");
        copy_into_history(&paths, "default", 6, Some(start)).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 2, "taken before it");
        // Its layout is the newest copy's now: not copied twice.
        copy_into_history(&paths, "default", 7, Some(start)).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 2, "the same layout");
        let newest = copies_of(&paths, "default").pop().unwrap().1;
        let kept: Snapshot = serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap();
        assert_eq!(kept, two);
    }

    /// `save_for_server` copies the file it replaces when the server started after it was taken, with
    /// a first copy a minute old; `save` does not.
    #[test]
    fn save_for_server_keeps_the_last_layout_of_a_server_that_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        let two = Snapshot {
            taken_unix: 1_700_000_500,
            ..labelled("two")
        };
        save(&paths, "default", &two).unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 1, "inside the wait");
        let three = Snapshot {
            taken_unix: 1_700_000_900,
            ..labelled("three")
        };
        save_for_server(&paths, "default", &three, Some(1_700_001_000)).unwrap();
        let kept: Vec<Snapshot> = copies_of(&paths, "default")
            .iter()
            .map(|(_, p)| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect();
        assert_eq!(
            kept,
            vec![sample(), two],
            "the layout before the server was kept"
        );
    }

    /// The names of the copies keep creation order when the clock reads earlier than the newest.
    #[test]
    fn a_copys_name_keeps_creation_order_across_a_clock_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        let path = snapshot_path(&paths, "default");
        std::fs::write(&path, serde_json::to_vec(&sample()).unwrap()).unwrap();
        copy_into_history(&paths, "default", 1_000_000, None).unwrap();
        let first = copies_of(&paths, "default").pop().unwrap().1;
        set_modified(&first, UNIX_EPOCH);
        std::fs::write(&path, serde_json::to_vec(&labelled("two")).unwrap()).unwrap();
        copy_into_history(&paths, "default", 500, None).unwrap();

        let stamps: Vec<u128> = copies_of(&paths, "default")
            .iter()
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(stamps, vec![1_000_000, 1_000_001]);
    }

    /// A copy name that is taken is never overwritten, nor is one whose pending file is: the
    /// caller then takes the next sequence number.
    #[test]
    fn a_copy_never_overwrites_one_that_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        let taken = tmp.path().join("session-default-1-1-0.json");
        std::fs::write(&taken, b"mine").unwrap();
        let err = write_new_copy(&taken, b"new").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&taken).unwrap(), b"mine");
        assert!(
            !taken.with_extension("pending").exists(),
            "no pending file left"
        );

        let racing = tmp.path().join("session-default-2-1-0.json");
        std::fs::write(racing.with_extension("pending"), b"half").unwrap();
        let err = write_new_copy(&racing, b"new").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(racing.with_extension("pending")).unwrap(),
            b"half"
        );
        assert!(!racing.exists());

        let fresh = tmp.path().join("session-default-3-1-0.json");
        write_new_copy(&fresh, b"new").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        assert!(!fresh.with_extension("pending").exists());
    }

    /// A delete copies the file it removes first (herdr copies before its `clear`), when the
    /// layout is one no copy holds; the copy stays, and a delete of what a copy holds adds none.
    #[test]
    fn delete_keeps_a_copy_of_a_layout_no_copy_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        let held = copies_of(&paths, "default").pop().unwrap().1;
        set_modified(&held, UNIX_EPOCH);
        // A layout a save never saw, put there as an older taw or another process might.
        std::fs::write(
            snapshot_path(&paths, "default"),
            serde_json::to_vec(&labelled("edited by hand")).unwrap(),
        )
        .unwrap();
        delete(&paths, "default").unwrap();
        let files = copies_of(&paths, "default");
        assert_eq!(files.len(), 2);
        let newest: Snapshot =
            serde_json::from_slice(&std::fs::read(&files[1].1).unwrap()).unwrap();
        assert_eq!(newest, labelled("edited by hand"));
        assert_eq!(
            std::fs::read(&held).unwrap(),
            serde_json::to_vec_pretty(&sample()).unwrap()
        );

        // What a copy already holds is not copied again.
        save(&paths, "default", &labelled("edited by hand")).unwrap();
        for (_, path) in copies_of(&paths, "default") {
            set_modified(&path, UNIX_EPOCH);
        }
        delete(&paths, "default").unwrap();
        assert_eq!(copies_of(&paths, "default").len(), 2);
    }

    /// A save copies the file it is about to replace, not only what it writes: with a layout no
    /// copy holds on disk, the copy that is made is that layout, and the cadence then blocks the
    /// copy of what is written.
    #[test]
    fn a_save_copies_the_file_it_replaces_before_it_replaces_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        std::fs::write(
            snapshot_path(&paths, "default"),
            serde_json::to_vec(&labelled("on disk")).unwrap(),
        )
        .unwrap();
        save(&paths, "default", &labelled("written")).unwrap();
        let files = copies_of(&paths, "default");
        assert_eq!(files.len(), 1);
        let copy: Snapshot = serde_json::from_slice(&std::fs::read(&files[0].1).unwrap()).unwrap();
        assert_eq!(copy, labelled("on disk"));
    }

    /// `list` and `delete` never see session-snapshots/: a name a copy has is not a saved
    /// session, and copies left behind do not make one.
    #[test]
    fn list_and_delete_never_see_the_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        save(&paths, "default", &sample()).unwrap();
        let copy = copies_of(&paths, "default").pop().unwrap().1;
        let copy_name = copy.file_stem().unwrap().to_str().unwrap().to_string();

        let names: Vec<String> = list(&paths).unwrap().into_iter().map(|i| i.name).collect();
        assert_eq!(names, vec!["default"]);
        for refused in [copy_name.clone(), format!("session-snapshots/{copy_name}")] {
            let err = delete(&paths, &refused).unwrap_err();
            assert!(is_not_found(&err), "{err:#}");
        }
        assert!(copy.exists());

        delete(&paths, "default").unwrap();
        assert!(copy.exists(), "a delete leaves the copies");
        assert!(list(&paths).unwrap().is_empty());
    }

    /// A pane listed in `cwd`, at `index` in its window, with no agent.
    fn listed_in(cwd: &str, index: u32) -> PaneInfo {
        PaneInfo {
            pane_id: "%7".to_string(),
            session_id: "$1".to_string(),
            session_name: "alpha".to_string(),
            session_path: "/repo".to_string(),
            session_attached: 1,
            window_id: "@1".to_string(),
            window_index: 0,
            window_name: "editor".to_string(),
            window_active: true,
            window_layout: "abcd,80x24,0,0,0".to_string(),
            pane_index: index,
            pane_active: false,
            pane_pid: 4242,
            pane_current_command: "zsh".to_string(),
            pane_current_path: cwd.to_string(),
            pane_title: String::new(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: crate::store::AgentRecord::default(),
        }
    }

    /// A pane a restore left as tmux made it, its saved directory gone, is saved as it was saved: its
    /// directory, its agent, session and name, and the command its agent reported, and not the home
    /// folder tmux opened it in, nor what the pane holds now; in its place in the window as it is now.
    #[test]
    fn a_pane_a_restore_left_as_tmux_made_it_is_saved_as_it_was_saved() {
        let saved = PaneSnap {
            index: 0,
            cwd: "/home/me/gone project".to_string(),
            active: true,
            agent_kind: Some("claude".to_string()),
            agent_session_id: Some("conv-1".to_string()),
            agent_session_path: Some("/home/me/.claude/conv-1.jsonl".to_string()),
            agent_name: Some("keeper".to_string()),
            agent_kind_hint: Some("claude".to_string()),
            agent_resume: Some(crate::agent_resume::ReportedResume {
                source: "integration:claude".to_string(),
                agent: "claude".to_string(),
                argv: ["claude", "--resume", "conv-1"].map(String::from).to_vec(),
            }),
            agent_sandbox: None,
            agent_container: None,
            agent_claude_flags: Vec::new(),
        };
        let mut pane = listed_in("/home/me", 2);
        pane.agent.restore_error = Some("Saved directory is unavailable.".to_string());
        pane.agent.restore_saved = Some(serde_json::to_string(&saved).unwrap());
        // Whatever the pane has come to hold since: another agent, a name of its own.
        pane.agent.kind = Some("codex".to_string());
        pane.agent.session_id = Some("other".to_string());
        pane.agent.name = Some("newcomer".to_string());

        assert_eq!(
            pane_snap(&pane),
            PaneSnap {
                index: 2,
                active: false,
                ..saved
            }
        );
    }

    /// A pane a restore kept as it was saved keeps all it was saved with, its session, profile and
    /// runtime included, whatever runs in it now (a container typed by hand, a profile, a plain agent):
    /// what was saved of its agent is what a restore decides on (`restore::plan_of`).
    #[test]
    fn a_kept_pane_keeps_all_it_was_saved_with_whatever_runs_in_it_now() {
        let saved = PaneSnap {
            index: 0,
            cwd: "/home/me/gone project".to_string(),
            active: true,
            agent_kind: Some("claude".to_string()),
            agent_session_id: Some("conv-1".to_string()),
            agent_session_path: Some("/home/me/.claude/conv-1.jsonl".to_string()),
            agent_name: Some("keeper".to_string()),
            agent_kind_hint: Some("claude".to_string()),
            agent_resume: Some(crate::agent_resume::ReportedResume {
                source: "integration:claude".to_string(),
                agent: "claude".to_string(),
                argv: ["claude", "--resume", "conv-1"].map(String::from).to_vec(),
            }),
            agent_sandbox: None,
            agent_container: None,
            agent_claude_flags: Vec::new(),
        };
        let kept_with = |command: &str, container: Option<&str>, sandbox: Option<&str>| {
            let mut pane = listed(command, container, sandbox);
            pane.agent.restore_error = Some("Saved directory is unavailable.".to_string());
            pane.agent.restore_saved = Some(serde_json::to_string(&saved).unwrap());
            pane_snap(&pane)
        };

        for (command, container, sandbox) in [
            ("docker", Some("docker"), Some("default")),
            ("docker", Some("docker"), None),
            ("docker", None, None),
            ("bash", None, Some("default")),
            ("claude", None, None),
        ] {
            assert_eq!(
                kept_with(command, container, sandbox),
                saved,
                "{command} {container:?} {sandbox:?}"
            );
        }
        let sandboxed = PaneSnap {
            agent_sandbox: Some("max".to_string()),
            agent_container: Some("docker".to_string()),
            agent_claude_flags: Vec::new(),
            ..saved.clone()
        };
        let mut pane = listed("bash", None, None);
        pane.agent.restore_saved = Some(serde_json::to_string(&sandboxed).unwrap());
        assert_eq!(pane_snap(&pane), sandboxed);
    }

    /// What a restore kept of a pane before format 2 (`@agent_restore_saved`, read back across taw
    /// updates) has no profile and no runtime in it, and still reads as the pane it kept: without it
    /// the keep would end without a word, and the next save would hold the home folder tmux opened the
    /// pane in, and no session.
    #[test]
    fn a_pane_a_restore_kept_before_format_2_is_still_saved_as_it_was_saved() {
        let kept_before = r#"{"index":0,"cwd":"/home/me/gone project","active":true,"agent_kind":"claude","agent_session_id":"conv-1","agent_session_path":null,"agent_name":"keeper","agent_kind_hint":null}"#;
        let mut pane = listed_in("/home/me", 1);
        pane.agent.restore_error = Some("Saved directory is unavailable.".to_string());
        pane.agent.restore_saved = Some(kept_before.to_string());

        let saved = pane_snap(&pane);

        assert_eq!(
            (
                saved.cwd.as_str(),
                saved.index,
                saved.agent_kind.as_deref(),
                saved.agent_session_id.as_deref(),
                saved.agent_name.as_deref()
            ),
            (
                "/home/me/gone project",
                1,
                Some("claude"),
                Some("conv-1"),
                Some("keeper")
            )
        );
        assert_eq!((saved.agent_sandbox, saved.agent_container), (None, None));
    }

    /// A pane with no such mark is saved as it is, and so is one whose mark does not read as a saved
    /// pane: a pane is never saved as something it was not.
    #[test]
    fn a_pane_with_no_kept_self_that_reads_is_saved_as_it_is() {
        let mut pane = listed_in("/home/me/project", 1);
        pane.agent.kind = Some("claude".to_string());
        pane.agent.session_id = Some("conv-2".to_string());
        let as_it_is = pane_snap(&pane);
        assert_eq!(
            (
                as_it_is.cwd.as_str(),
                as_it_is.index,
                as_it_is.agent_kind.as_deref(),
                as_it_is.agent_session_id.as_deref()
            ),
            ("/home/me/project", 1, Some("claude"), Some("conv-2"))
        );
        for unreadable in ["", "not a pane", "{\"cwd\":\"/gone\"}"] {
            pane.agent.restore_saved = Some(unreadable.to_string());
            assert_eq!(pane_snap(&pane), as_it_is, "{unreadable:?}");
        }
    }

    /// The flags a pane's Claude was started with are saved from the pane's record, and a pane that has
    /// none saves the JSON it saved before there were any.
    #[test]
    fn a_save_keeps_the_claude_flags_of_the_pane_and_a_pane_with_none_saves_no_key() {
        let mut pane = listed_in("/home/me/project", 1);
        pane.agent.kind = Some("claude".to_string());
        pane.agent.session_id = Some("conv-2".to_string());
        let plain = pane_snap(&pane);
        assert!(plain.agent_claude_flags.is_empty());
        assert!(
            !serde_json::to_string(&plain)
                .unwrap()
                .contains("claude_flags")
        );

        pane.agent.claude_flags = ["--effort", "max"].map(String::from).to_vec();
        let flagged = pane_snap(&pane);
        assert_eq!(flagged.agent_claude_flags, ["--effort", "max"]);
        assert!(
            serde_json::to_string(&flagged)
                .unwrap()
                .contains(r#""agent_claude_flags":["--effort","max"]"#)
        );
        let back: PaneSnap =
            serde_json::from_str(&serde_json::to_string(&flagged).unwrap()).unwrap();
        assert_eq!(back, flagged);
    }

    /// A snapshot written before the flags were kept, and the pane an older taw kept in
    /// `@agent_restore_saved`, have no such field: both still parse, with no flags, and a save keeps
    /// the pane as it was saved.
    #[test]
    fn a_snapshot_and_a_kept_pane_saved_before_the_flags_parse_with_none() {
        let old = r#"{"index":0,"cwd":"/a","active":true,"agent_kind":"claude","agent_session_id":"x","agent_session_path":null,"agent_name":null,"agent_kind_hint":null}"#;
        let pane: PaneSnap = serde_json::from_str(old).unwrap();
        assert!(pane.agent_claude_flags.is_empty());
        let snapshot = format!(
            r#"{{"version":1,"taken_unix":1,"sessions":[{{"name":"s","path":"/a","active_window_index":0,"windows":[{{"index":0,"name":"w","layout":"l","active":true,"panes":[{old}]}}]}}]}}"#
        );
        let snapshot: Snapshot = serde_json::from_str(&snapshot).unwrap();
        assert!(
            snapshot.sessions[0].windows[0].panes[0]
                .agent_claude_flags
                .is_empty()
        );

        let mut pane_info = listed_in("/home/me/project", 1);
        pane_info.agent.restore_saved = Some(old.to_string());
        let kept = pane_snap(&pane_info);
        assert_eq!(kept.cwd, "/a");
        assert!(kept.agent_claude_flags.is_empty());
        // The flags of a kept pane are the ones it was saved with, whatever the pane holds now.
        pane_info.agent.claude_flags = ["--model", "x"].map(String::from).to_vec();
        assert!(pane_snap(&pane_info).agent_claude_flags.is_empty());
        let with = old.replace(
            r#""agent_kind_hint":null"#,
            r#""agent_kind_hint":null,"agent_claude_flags":["--effort","max"]"#,
        );
        pane_info.agent.restore_saved = Some(with);
        assert_eq!(
            pane_snap(&pane_info).agent_claude_flags,
            ["--effort", "max"]
        );
    }

    fn at(path: &str, taken_unix: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            taken_unix,
        }
    }

    /// `default.json` is the layout when it was taken before the server started.
    #[test]
    fn the_look_back_takes_default_when_it_was_taken_before_the_server_started() {
        let default = at("default.json", 900);
        let copies = [at("copy-a", 100), at("copy-b", 800)];
        assert_eq!(
            newest_before(1000, Some(&default), &copies),
            Some(default.clone())
        );
        // Strictly before: taken as the server started is not.
        assert_eq!(
            newest_before(900, Some(&default), &copies),
            Some(at("copy-b", 800))
        );
    }

    /// With `default.json` taken after the server started, the newest copy from before it is.
    #[test]
    fn the_look_back_takes_the_newest_copy_from_before_the_start_when_default_is_newer() {
        let default = at("default.json", 1500);
        let copies = [
            at("copy-a", 100),
            at("copy-c", 950),
            at("copy-b", 400),
            at("copy-d", 1200),
        ];
        // By each layout's own time, not by the order of the files.
        assert_eq!(
            newest_before(1000, Some(&default), &copies),
            Some(at("copy-c", 950))
        );
        // No default.json at all: the same.
        assert_eq!(newest_before(1000, None, &copies), Some(at("copy-c", 950)));
    }

    /// A copy whose file was made after the server started (a server-change copy is named after the
    /// new server started) still holds a layout from before it, and is chosen by that.
    #[test]
    fn the_look_back_reads_a_copys_own_time_and_not_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let start = 1_700_000_100;
        // The layout of the old server, taken before the start, then replaced by the new
        // server's own, taken after it.
        let old = Snapshot {
            taken_unix: 1_700_000_060,
            ..labelled("old")
        };
        let new = Snapshot {
            taken_unix: 1_700_000_200,
            ..labelled("new")
        };
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        std::fs::write(
            snapshot_path(&paths, "default"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        save_for_server(&paths, "default", &new, Some(start)).unwrap();

        let files = copies_of(&paths, "default");
        assert_eq!(files.len(), 1);
        // Named after now, which is after the start: its name cannot tell.
        assert!(files[0].0 > u128::from(start) * 1_000_000_000);
        let chosen = look_back(&paths, "default", start).unwrap();
        assert_eq!(chosen.path, files[0].1);
        assert_eq!(chosen.taken_unix, 1_700_000_060);
        let loaded = load_file(&chosen.path, "default").unwrap();
        assert_eq!(loaded, old);
    }

    /// Nothing was taken before the start: nothing is chosen, and no older layout is looked for
    /// past one that is there.
    #[test]
    fn the_look_back_finds_nothing_when_nothing_was_taken_before_the_start() {
        let default = at("default.json", 1500);
        assert_eq!(newest_before(1000, Some(&default), &[at("c", 1000)]), None);
        assert_eq!(newest_before(1000, None, &[]), None);
        assert_eq!(newest_before(1000, Some(&default), &[]), None);
    }

    /// On disk: the copies of other names are never taken for `default`'s, and a file a newer taw
    /// wrote or that does not parse is left out, with a good copy under it chosen.
    #[test]
    fn the_look_back_reads_only_the_copies_of_its_name_and_only_layouts_it_can_use() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        let copies_dir = paths.sessions_dir.join(SNAPSHOTS_DIR);
        std::fs::create_dir_all(&copies_dir).unwrap();
        let put = |name: &str, snap: &Snapshot| {
            std::fs::write(copies_dir.join(name), serde_json::to_vec(snap).unwrap()).unwrap();
        };
        let at_time = |taken_unix| Snapshot {
            taken_unix,
            ..sample()
        };
        let stamp = |n: u32| format!("{n:039}-1-0");
        put(&format!("session-default-{}.json", stamp(1)), &at_time(500));
        put(&format!("session-default-{}.json", stamp(2)), &at_time(700));
        // Another name's, though its prefix is `session-default-`.
        put(
            &format!("session-default-2024-{}.json", stamp(3)),
            &at_time(900),
        );
        put(&format!("session-other-{}.json", stamp(4)), &at_time(950));
        // Newer than this taw reads, and not a layout at all.
        put(
            &format!("session-default-{}.json", stamp(5)),
            &Snapshot {
                version: CURRENT_VERSION + 1,
                ..at_time(990)
            },
        );
        std::fs::write(
            copies_dir.join(format!("session-default-{}.json", stamp(6))),
            b"garbage",
        )
        .unwrap();
        let chosen = look_back(&paths, "default", 1000).unwrap();
        assert_eq!(chosen.taken_unix, 700);
        assert_eq!(
            chosen.path,
            copies_dir.join(format!("session-default-{}.json", stamp(2)))
        );
    }
}
