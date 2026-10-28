// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! herdr's git-worktree workflow: `create`/`open`/`remove`/`list`. Each worktree is a
//! `git worktree` under `[worktrees] directory` plus a tmux session (named
//! `<repo-basename>--<sanitized-branch>`) rooted there.
//!
//! `trust_repository`, on `create`/`open`/`remove`, is ported from herdr's
//! `repository_git_args`/`repository_git_command` (`<herdr-src>/src/worktree.rs:154-170`):
//! when set, every git invocation this module makes is prefixed with
//! `-c safe.directory=<repo_root>`, trusting a repository git would otherwise call
//! "dubious" ownership on (a worktree on a shared filesystem, under a container
//! bind-mount, …) for that one invocation, rather than requiring a standing
//! `git config --global --add safe.directory` on the host. `open` accepts the flag for
//! parity with herdr's CLI (and with `create`/`remove`) but has nothing to apply it to:
//! unlike herdr's `open`, this one makes no git calls at all — it trusts the persisted
//! [`WorktreeRecord`] and only touches tmux.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::paths::Paths;
use crate::plugin::{self, PluginCtx};
use crate::tmux::{Tmux, ids};

/// One worktree `create`d (or recreated by `open`), persisted in `<state>/worktrees.json`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WorktreeRecord {
    /// Public id of the session backing this worktree *at creation time* (e.g. `w7`).
    /// Stable across restarts even though the session's actual tmux id can change when
    /// `open` has to recreate it — this, the session name, and the branch are the three
    /// keys `open`/`remove` accept.
    pub workspace_id: String,
    pub session_name: String,
    pub path: PathBuf,
    pub branch: String,
    pub base: Option<String>,
    pub repo_root: PathBuf,
    pub source_workspace_id: String,
    pub created_unix_ms: u64,
    /// The machine of the config this worktree is on: an index into
    /// [`crate::tui::data::Snapshot::machines`], `None` for the worktrees of the machine taw runs
    /// on, which are the ones `worktrees.json` holds. Never saved: a record read from the file is
    /// always this machine's, and a record of another machine only ever exists in memory, merged
    /// into the popup's list ([`crate::tui::machines::merge_worktrees`]).
    #[serde(skip)]
    pub machine: Option<usize>,
    /// For a record of another machine: `path` and `repo_root` resolved there, when the feed sent
    /// ones that differ. Matching only; skipped like `machine`, so `worktrees.json` keeps its format.
    #[serde(skip)]
    pub real_path: Option<PathBuf>,
    #[serde(skip)]
    pub real_repo_root: Option<PathBuf>,
}

impl WorktreeRecord {
    /// The checkout as it is compared with a session's folder: resolved on its machine when the feed
    /// said so, else as recorded.
    pub fn path_to_match(&self) -> &Path {
        self.real_path.as_deref().unwrap_or(&self.path)
    }

    /// The repository root as it is compared with a session's folder, like [`Self::path_to_match`].
    pub fn repo_root_to_match(&self) -> &Path {
        self.real_repo_root.as_deref().unwrap_or(&self.repo_root)
    }
}

/// Errors `worktree::{create,open,remove}` report as a specific, matchable code (via
/// `Error::downcast`) rather than only a formatted message — mirrors
/// `plugin::install::InstallError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    #[error("not_a_git_repo: '{0}' is not inside a git repository")]
    NotAGitRepo(String),
    #[error("session_exists: a tmux session named '{0}' already exists")]
    SessionExists(String),
    #[error("worktree_not_found: no worktree matches '{0}'")]
    NotFound(String),
    /// `open` of a worktree whose session is not running and whose checkout folder is gone (removed
    /// by hand, or by git): tmux would make the session in the home folder and say nothing. herdr
    /// does not open such a worktree either (`worktree_not_found`).
    #[error("worktree_not_found: the checkout of worktree {branch} is gone: {}", path.display())]
    CheckoutGone { branch: String, path: PathBuf },
    #[error("invalid branch name: {0}")]
    InvalidBranch(String),
    /// git would not remove the worktree, and a forced removal would. `message` is git's own
    /// failure, worded as any other failed git call here is.
    #[error("{message}")]
    RemoveRefused {
        why: RemoveRefusal,
        /// The worktree's branch, for whoever asks the user.
        branch: String,
        /// The worktree's folder, which a forced removal deletes.
        path: PathBuf,
        message: String,
    },
}

/// Why a removal without `force` was refused, where `force` would have gone through
/// (herdr v0.9.1, #3766 and #1797: dirty and submodule worktrees offer a forced removal, and
/// a removal that failed partway can be retried).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveRefusal {
    /// The checkout has modified or untracked files, which a removal deletes.
    Dirty,
    /// The checkout holds submodules, which git removes only when forced.
    Submodules,
    /// git no longer lists the worktree but its folder is still there: an earlier removal
    /// failed partway, having let go of the worktree but not of every file in it.
    Leftover,
}

/// Rejects a branch name git or a shell would mishandle, before either is handed it.
///
/// A new-branch worktree runs `git worktree add -b <branch> …`; git reads a `<branch>`
/// beginning with `-` as one of its own options, so `--branch -x` (or `--branch=--base`)
/// dumped git's multi-line usage text instead of a clean error. The name also becomes a
/// worktree directory and a tmux session name, so whitespace and shell metacharacters have
/// no business in it either. All are refused as [`WorktreeError::InvalidBranch`], which the
/// CLI reports as `invalid_argument`. The ordinary stuff of a branch name — `feature/foo`,
/// `release/1.2`, `fix-bug_3` — is deliberately allowed.
pub fn validate_branch_name(branch: &str) -> Result<(), WorktreeError> {
    let bad = |m: String| Err(WorktreeError::InvalidBranch(m));
    if branch.is_empty() {
        return bad("must not be empty".to_string());
    }
    if branch.starts_with('-') {
        return bad(format!(
            "must not start with '-' (git reads {branch:?} as an option)"
        ));
    }
    if let Some(c) = branch
        .chars()
        .find(|c| c.is_whitespace() || is_branch_hostile(*c))
    {
        return bad(format!("must not contain {c:?}: {branch:?}"));
    }
    Ok(())
}

/// Characters a shell or git's ref parser treats specially; none belong in a name that also
/// becomes a filesystem directory and a tmux session.
fn is_branch_hostile(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | '`'
            | '$'
            | '\\'
            | ';'
            | '&'
            | '|'
            | '<'
            | '>'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '*'
            | '?'
            | '!'
            | '~'
            | '^'
            | ':'
            | '#'
    ) || c.is_control()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct RecordsFile {
    #[serde(default)]
    worktrees: Vec<WorktreeRecord>,
}

fn records_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("worktrees.json")
}

fn load_records(paths: &Paths) -> anyhow::Result<Vec<WorktreeRecord>> {
    let path = records_path(paths);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: RecordsFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file.worktrees)
}

/// Writes `worktrees.json` atomically (`.tmp` file, then rename over the real path).
fn save_records(paths: &Paths, records: &[WorktreeRecord]) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("creating {}", paths.state_dir.display()))?;
    let path = records_path(paths);
    let tmp = path.with_extension("json.tmp");
    let file = RecordsFile {
        worktrees: records.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file).context("serializing worktrees.json")?;
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

fn find_record(records: &[WorktreeRecord], key: &str) -> Option<usize> {
    records
        .iter()
        .position(|r| r.workspace_id == key || r.session_name == key || r.branch == key)
}

/// Creates a new worktree from `workspace`'s repository: resolves `workspace` (any id/
/// name/target `tmux::ids::resolve_session` accepts) to a session, finds its repo root,
/// checks the target session name is free *before* touching git, creates the git worktree
/// (checking out `branch` if it already exists locally, else branching it from `base` or
/// `HEAD`), opens a tmux session on it, persists a [`WorktreeRecord`], and emits
/// `worktree.created`.
#[allow(clippy::too_many_arguments)]
pub fn create(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    workspace: &str,
    branch: &str,
    base: Option<&str>,
    focus: bool,
    trust_repository: bool,
) -> anyhow::Result<WorktreeRecord> {
    // Before git or tmux is touched: a dash-leading, blank, or shell-hostile branch name is
    // rejected cleanly rather than reaching git as a flag (which dumped its usage text).
    validate_branch_name(branch)?;
    let source_session_id = ids::resolve_session(t, workspace)?;
    let source_session = t
        .list_sessions()?
        .into_iter()
        .find(|s| s.session_id == source_session_id)
        .ok_or_else(|| anyhow::anyhow!("workspace '{workspace}' not found"))?;

    let repo_root = git_toplevel(&source_session.session_path, trust_repository)
        .ok_or_else(|| WorktreeError::NotAGitRepo(source_session.session_path.clone()))?;
    let repo_basename = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo");
    // Two names, deliberately different.
    //
    // `dir_name` is `<repo>--<branch>`, with only `/` folded so it is one path
    // segment. It names a *directory*, where `.` and `:` are ordinary characters — a
    // worktree for `release/1.2` lives at `repo--release-1.2`, which is what a user
    // reading `worktrees.directory` expects to find.
    //
    // `session_name` is that put through the one rule for what tmux does to a session
    // name (`.` and `:` become `_`). Without it the free-name check below tests a name
    // nothing will ever be called and every read-back looks for a session that does not
    // exist under that spelling. Both a dot-prefixed repo (`~/.dotfiles`) and a dotted
    // branch hit that.
    let dir_name = format!("{repo_basename}--{}", sanitize_branch(branch));
    let session_name = crate::tmux::session_name(&dir_name);

    if t.list_sessions()?
        .iter()
        .any(|s| s.session_name == session_name)
    {
        anyhow::bail!(WorktreeError::SessionExists(session_name));
    }

    let dir = cfg.worktrees.directory.join(&dir_name);
    std::fs::create_dir_all(&cfg.worktrees.directory)
        .with_context(|| format!("creating {}", cfg.worktrees.directory.display()))?;
    let dir_str = path_str(&dir)?;

    // `--` before the positionals, always. `branch` and `base` come straight from
    // `--branch`/`--base`, and git reads a leading `-` as an option: `--base --lock` was
    // consumed as `git worktree add`'s own `--lock` flag, so the commit-ish silently
    // defaulted to HEAD *and* the worktree was created locked — which then made
    // `worktree remove` refuse to remove it without `--force`. (`-b <branch>` is an
    // option's value rather than a positional, and git's own parser already rejects a
    // dash-leading value there.)
    if branch_exists(&repo_root, branch, trust_repository) {
        run_git(
            &repo_root,
            &["worktree", "add", "--", dir_str, branch],
            trust_repository,
        )?;
    } else {
        run_git(
            &repo_root,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                "--",
                dir_str,
                base.unwrap_or("HEAD"),
            ],
            trust_repository,
        )?;
    }

    // The name tmux answers with, not the one asked for: tmux rewrites some session names
    // and refuses others, and everything below — the record, `open`, `remove` — needs the
    // name the session really has.
    //
    // From here on every failure rolls back *everything this call made*, not just the last
    // step. The half-built states are all unreachable afterwards: a git worktree with no
    // record is a directory `open`/`remove` cannot find, and a live tmux session with no
    // record is a workspace nothing associates with a worktree — so a failure at the
    // read-back or at `save_records` would leave both behind for good. The branch is
    // never undone: `remove` does not delete branches either, and `branch` may well have
    // existed before this call.
    let created = t.run(&[
        "new-session",
        "-d",
        "-s",
        &crate::tmux::new_name(&session_name),
        "-c",
        &crate::tmux::start_directory(dir_str),
        "-P",
        "-F",
        "#{session_name}",
    ]);
    let session_name = match created {
        Ok(created) => created.trim().to_string(),
        Err(err) => {
            rollback_worktree(&repo_root, dir_str, trust_repository);
            return Err(err.context(format!(
                "creating the tmux session for worktree '{session_name}'"
            )));
        }
    };
    if focus {
        let _ = t.run_ok(&[
            "switch-client",
            "-t",
            &crate::tmux::session_target(&session_name),
        ]);
    }

    // Everything after the session exists, with the session in the rollback set too.
    let finish = |t: &Tmux| -> anyhow::Result<WorktreeRecord> {
        let new_session_id = t
            .list_sessions()?
            .into_iter()
            .find(|s| s.session_name == session_name)
            .map(|s| s.session_id)
            .ok_or_else(|| {
                anyhow::anyhow!("session '{session_name}' vanished right after creation")
            })?;
        Ok(WorktreeRecord {
            workspace_id: ids::workspace_id(&new_session_id),
            session_name: session_name.clone(),
            path: dir.clone(),
            branch: branch.to_string(),
            base: base.map(str::to_string),
            repo_root: repo_root.clone(),
            source_workspace_id: ids::workspace_id(&source_session_id),
            created_unix_ms: now_unix_ms(),
            machine: None,
            real_path: None,
            real_repo_root: None,
        })
    };
    let record = match finish(t).and_then(|record| {
        let mut records = load_records(paths)?;
        records.push(record.clone());
        save_records(paths, &records)?;
        Ok(record)
    }) {
        Ok(record) => record,
        Err(err) => {
            let _ = t.run_ok(&["kill-session", "-t", &session_name]);
            rollback_worktree(&repo_root, dir_str, trust_repository);
            return Err(err.context(format!(
                "recording the worktree for session '{session_name}'"
            )));
        }
    };

    emit_event(
        t,
        cfg,
        paths,
        taw_bin,
        "worktree.created",
        &session_name,
        &record,
    );

    Ok(record)
}

/// Opens an existing worktree, matched by public id, session name, or branch. If its
/// session is already running, returns it with `already_open = true` (never
/// duplicated/altered); otherwise recreates the session from the record's saved path and
/// returns `already_open = false`. Emits `worktree.opened` either way. A worktree whose
/// session is not running and whose checkout is gone is [`WorktreeError::CheckoutGone`],
/// and no session is made.
pub fn open(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    workspace_or_branch: &str,
    focus: bool,
    trust_repository: bool,
) -> anyhow::Result<(WorktreeRecord, bool)> {
    // Accepted for CLI parity with herdr's `worktree open --trust-repository` (and with
    // `create`/`remove` here) but unused: this `open` makes no git calls at all, unlike
    // herdr's — see the module doc comment.
    let _ = trust_repository;
    let records = load_records(paths)?;
    let idx = find_record(&records, workspace_or_branch)
        .ok_or_else(|| WorktreeError::NotFound(workspace_or_branch.to_string()))?;
    let record = records[idx].clone();

    let already_open = t
        .list_sessions()?
        .iter()
        .any(|s| s.session_name == record.session_name);

    if !already_open {
        // tmux makes a session whose directory is gone in the home folder, and says nothing.
        if !record.path.is_dir() {
            return Err(WorktreeError::CheckoutGone {
                branch: record.branch.clone(),
                path: record.path.clone(),
            }
            .into());
        }
        let dir_str = path_str(&record.path)?;
        t.run_ok(&[
            "new-session",
            "-d",
            "-s",
            &crate::tmux::new_name(&record.session_name),
            "-c",
            &crate::tmux::start_directory(dir_str),
        ])?;
    }

    if focus {
        let _ = t.run_ok(&[
            "switch-client",
            "-t",
            &crate::tmux::session_target(&record.session_name),
        ]);
    }

    emit_event(
        t,
        cfg,
        paths,
        taw_bin,
        "worktree.opened",
        &record.session_name,
        &record,
    );

    Ok((record, already_open))
}

/// Removes a worktree, matched the same way as [`open`]: `git worktree remove [--force]`
/// (its error propagates, leaving the tmux session and the record untouched, so the removal
/// can be tried again), kills the tmux session if it is still running, drops the record, and
/// emits `worktree.removed`. Never deletes the branch.
///
/// Three failures are told apart, because each has something a caller can do about it
/// (herdr v0.9.1, #3766, #3314 and #1797). A checkout git calls dirty, or one with
/// submodules, is refused without `force` as [`WorktreeError::RemoveRefused`]: a forced
/// removal goes through. A removal that failed partway, having let go of the worktree in
/// git's books but not of every file in its folder, leaves a folder git says is no worktree;
/// without `force` that is a refusal as well ([`RemoveRefusal::Leftover`]), and with it the
/// folder is cleared, but only if its own `.git` file says it was this repository's
/// checkout. A worktree that is gone altogether, folder and all, is finished off: the
/// session and the record go. `[hooks] pre_worktree_remove` is asked before any of it, forced
/// or not.
pub fn remove(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    workspace: &str,
    force: bool,
    trust_repository: bool,
) -> anyhow::Result<WorktreeRecord> {
    let mut records = load_records(paths)?;
    let idx = find_record(&records, workspace)
        .ok_or_else(|| WorktreeError::NotFound(workspace.to_string()))?;
    let record = records[idx].clone();

    // `[hooks] pre_worktree_remove` is asked before git is: a refusal leaves the checkout, the
    // branch, the session and the record as they were, and reaches the popup's toast and the
    // CLI's `hook_refused` as the hook's own failure.
    let ctx = PluginCtx {
        tmux: t,
        cfg,
        paths,
        taw_bin,
        server_env: None,
    };
    crate::hooks::pre_worktree_remove(&ctx, &record)?;

    let dir_str = path_str(&record.path)?;
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    // See `create`: every positional handed to git is terminated, so a path can never be
    // read as an option.
    args.push("--");
    args.push(dir_str);
    if let Err(failure) = git_capture(&record.repo_root, &args, trust_repository) {
        settle_failed_removal(&record, force, trust_repository, failure)?;
    }

    if t.list_sessions()?
        .iter()
        .any(|s| s.session_name == record.session_name)
    {
        t.run_ok(&["kill-session", "-t", &record.session_name])?;
    }

    records.remove(idx);
    save_records(paths, &records)?;

    let payload = serde_json::json!({
        "workspace_id": record.workspace_id,
        "worktree": &record,
        "forced": force,
    });
    if let Err(err) = plugin::events::emit(&ctx, "worktree.removed", payload) {
        log::warn!("worktree.removed event: {err:#}");
    }

    Ok(record)
}

/// Every tracked worktree, dropping (and pruning from `worktrees.json`) any record whose
/// directory no longer exists on disk.
pub fn list(_t: &Tmux, paths: &Paths) -> anyhow::Result<Vec<WorktreeRecord>> {
    let mut records = load_records(paths)?;
    let before = records.len();
    records.retain(|r| r.path.exists());
    if records.len() != before {
        save_records(paths, &records)?;
    }
    Ok(records)
}

/// A running worktree session created from another workspace: see [`linked_sessions`].
#[derive(Clone, Debug)]
pub struct LinkedSession {
    /// The running session's own tmux id (`$7`) — not the record's `workspace_id`, which is
    /// the id the session had when the worktree was created.
    pub session_id: String,
    pub session_name: String,
    pub record: WorktreeRecord,
}

// Follows herdr's src/app/actions.rs (workspace_close_indices).
// Differences: herdr's group is every workspace sharing a repository key; here it is found
// from the worktree records (the workspace each was cut from, followed through closed
// worktrees), and only for a workspace that is not itself a worktree.
/// The running worktree sessions created from `source` and, since a worktree can itself be
/// the workspace another is cut from, from those in turn — herdr's "linked worktree
/// workspaces" of a primary workspace (`workspace_close_indices`, v0.9.1, #2874): the group
/// `taw workspace close --group` closes along with it. Nearest first, each level in the
/// order the worktrees were created.
///
/// Only a workspace that is not itself a worktree has a group. herdr puts every worktree of
/// a repository in the group of its primary workspace and expands no group from a linked
/// worktree, so closing a worktree workspace closes just that one, even when another
/// worktree was cut from it (which stays open, and still closes with the primary).
///
/// A record names the workspace it was cut from by the public id that workspace had *then*
/// ([`WorktreeRecord::source_workspace_id`]), and is matched to its own session by name.
/// Only a session that is running counts: a closed worktree has nothing left to strand. It
/// does not cut the chain, though: the worktrees cut from it are still found through its
/// record, since herdr keeps every worktree of a repository in its primary's group and
/// closing one never takes another out of it.
/// tmux hands out session ids afresh after a server restart, so an old id can later name a
/// session that has nothing to do with the worktree; one whose directory is not inside the
/// record's repository is therefore never taken to be its source, whatever the id says.
pub fn linked_sessions(
    t: &Tmux,
    paths: &Paths,
    source: &crate::tmux::SessionInfo,
) -> anyhow::Result<Vec<LinkedSession>> {
    let records = load_records(paths)?;
    if records.is_empty()
        || records
            .iter()
            .any(|r| r.session_name == source.session_name)
    {
        return Ok(Vec::new());
    }
    let sessions = t.list_sessions()?;
    let running: Vec<SessionRef<'_>> = sessions
        .iter()
        .map(|s| SessionRef {
            id: &s.session_id,
            name: &s.session_name,
            path: Path::new(&s.session_path),
        })
        .collect();
    let source = SessionRef {
        id: &source.session_id,
        name: &source.session_name,
        path: Path::new(&source.session_path),
    };
    Ok(linked_among(&records, &running, source))
}

/// A running session as [`linked_among`] reads it: its tmux id, its name and its directory.
/// Borrowed, so whatever holds the sessions (tmux's own listing, the popup's snapshot) lends
/// them without a copy.
#[derive(Clone, Copy, Debug)]
pub struct SessionRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub path: &'a Path,
}

/// [`linked_sessions`] over what the caller already holds: the worktree `records` and the
/// `sessions` running. No tmux and no file is read (the paths are compared by what they
/// resolve to, where they do). The popup asks this of the data it already has, as `x` on a
/// session header, which decides what it asks, cannot wait on a listing.
pub fn linked_among(
    records: &[WorktreeRecord],
    sessions: &[SessionRef<'_>],
    source: SessionRef<'_>,
) -> Vec<LinkedSession> {
    if records.iter().any(|r| r.session_name == source.name) {
        return Vec::new();
    }
    let mut linked: Vec<LinkedSession> = Vec::new();
    // Where the chain is followed from, as (public id, directory): a running session by its
    // own id and directory, a closed worktree by the id it was created with and its
    // checkout.
    let mut pending = std::collections::VecDeque::from([(
        ids::workspace_id(source.id),
        source.path.to_path_buf(),
    )]);
    // Each record is followed once: `source` itself is never in the group, and a chain that
    // loops back on itself (a hand-edited `worktrees.json`) cannot spin.
    let mut followed = std::collections::HashSet::new();
    while let Some((from_id, from_path)) = pending.pop_front() {
        for record in records {
            if record.source_workspace_id != from_id
                || !path_is_within(
                    &from_path,
                    record.repo_root_to_match(),
                    record.machine.is_some(),
                )
                || !followed.insert(record.session_name.clone())
            {
                continue;
            }
            match sessions.iter().find(|s| s.name == record.session_name) {
                Some(running) if running.id == source.id => {}
                Some(running) => {
                    linked.push(LinkedSession {
                        session_id: running.id.to_string(),
                        session_name: running.name.to_string(),
                        record: record.clone(),
                    });
                    pending.push_back((ids::workspace_id(running.id), running.path.to_path_buf()));
                }
                None => pending.push_back((
                    record.workspace_id.clone(),
                    record.path_to_match().to_path_buf(),
                )),
            }
        }
    }
    linked
}

/// Whether `path` is `root` or inside it, by real path where both resolve: `git` reports a
/// repository's toplevel with symlinks resolved, while a session's directory is whatever it
/// was created with (`/var/…` and `/private/var/…` on macOS name the same place).
///
/// Another machine's paths (`on_machine`) are compared as that machine sent them, never resolved
/// here: its feed already sends them resolved where they differ, and this machine can only add
/// wrong answers for them (a path that exists here behind a symlink, or only one of the two existing).
fn path_is_within(path: &Path, root: &Path, on_machine: bool) -> bool {
    if on_machine {
        return path.starts_with(root);
    }
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real(path).starts_with(real(root))
}

/// Builds `{"workspace": <workspace_json>, "worktree": <record>}` and emits `event`,
/// logging (never failing the caller) if either the session lookup or the emit itself
/// errors — matches how plugin/startup-hook failures are handled elsewhere in this crate
/// (`plugin::run_startup_hooks`, `plugin::startup`): a notification problem must not
/// unwind a worktree operation that has already succeeded.
fn emit_event(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    event: &str,
    session_name: &str,
    record: &WorktreeRecord,
) {
    let workspace = match workspace_json_for(t, session_name) {
        Ok(w) => w,
        Err(err) => {
            log::warn!("{event} event: looking up session '{session_name}': {err:#}");
            return;
        }
    };
    let payload = serde_json::json!({ "workspace": workspace, "worktree": record });
    let ctx = PluginCtx {
        tmux: t,
        cfg,
        paths,
        taw_bin,
        server_env: None,
    };
    if let Err(err) = plugin::events::emit(&ctx, event, payload) {
        log::warn!("{event} event: {err:#}");
    }
}

fn workspace_json_for(t: &Tmux, session_name: &str) -> anyhow::Result<serde_json::Value> {
    let panes = t.list_panes()?;
    t.list_sessions()?
        .into_iter()
        .find(|s| s.session_name == session_name)
        .map(|s| plugin::context::workspace_json(&s, &panes))
        .ok_or_else(|| anyhow::anyhow!("session '{session_name}' not found"))
}

pub(crate) fn path_str(p: &Path) -> anyhow::Result<&str> {
    p.to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", p.display()))
}

fn sanitize_branch(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Undoes the `git worktree add` this call made, after the tmux session it was made for
/// could not be created. Best effort and logged rather than propagated: the caller is
/// already returning the real failure, and a rollback that itself fails must not replace
/// that message with a less useful one. `--force` because the worktree is seconds old and
/// empty of anything a user could have put there. The branch is deliberately left alone.
fn rollback_worktree(repo_root: &Path, dir: &str, trust_repository: bool) {
    if let Err(err) = run_git(
        repo_root,
        &["worktree", "remove", "--force", "--", dir],
        trust_repository,
    ) {
        log::warn!("could not roll back the git worktree at {dir}: {err:#}");
    }
}

/// Follows herdr's `repository_git_command`/`repository_git_args`
/// (`<herdr-src>/src/worktree.rs:154-170`): `-c safe.directory=<repo_root>` (when
/// `trust_repository`), then `-C <repo_root>`. Every git invocation against an already
/// -resolved `repo_root` builds on this; [`git_toplevel`] cannot (see its own comment) and
/// builds its `Command` directly.
fn git_command(repo_root: &Path, trust_repository: bool) -> Command {
    let mut command = crate::git::command();
    // A removal is told apart by what git says (`is_dirty_remove_error`), and git says it in
    // the user's language unless it is asked not to, as herdr asks it.
    command.env("LC_ALL", "C");
    if trust_repository {
        command
            .arg("-c")
            .arg(format!("safe.directory={}", repo_root.display()));
    }
    command.arg("-C").arg(repo_root);
    command
}

fn git_toplevel(dir: &str, trust_repository: bool) -> Option<PathBuf> {
    let mut command = crate::git::command();
    if trust_repository {
        // Unlike every other git call in this module, this one *is* the repo-root
        // discovery: there is no resolved value yet to scope `safe.directory` to, and
        // `dir` need not be the toplevel itself (it is the source session's cwd, which
        // may be any subdirectory), so it cannot stand in for one either. `*` (every
        // repository) is the only value that can unblock this specific call once the
        // caller has opted in to trust; herdr has no equivalent call to port, since its
        // own repo-root discovery (`workspace::git::discovery::git_repo_root`) walks the
        // filesystem directly rather than shelling out to git.
        command.arg("-c").arg("safe.directory=*");
    }
    let output = command
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// The id of the session of the repository that `dir` is in, made if there is none: herdr's
/// `worktree create --cwd <repo>` names a folder inside the repository, not a workspace. A session
/// belongs to the repository when its own folder is inside it (the first one listed wins); else one
/// is made at the repository's root and named for it. A folder that is in no git repository is
/// [`WorktreeError::NotAGitRepo`].
pub fn session_of_repository(
    t: &Tmux,
    dir: &str,
    trust_repository: bool,
) -> anyhow::Result<String> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Err(WorktreeError::NotAGitRepo(dir.to_string()).into());
    }
    let root = git_toplevel(dir, trust_repository)
        .ok_or_else(|| WorktreeError::NotAGitRepo(dir.to_string()))?;
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let root = canonical(&root);
    let sessions = t.list_sessions().unwrap_or_default();
    for session in &sessions {
        if session.session_path.is_empty() {
            continue;
        }
        if git_toplevel(&session.session_path, trust_repository)
            .is_some_and(|top| canonical(&top) == root)
        {
            return Ok(session.session_id.clone());
        }
    }
    let base =
        crate::tmux::session_name(root.file_name().and_then(|s| s.to_str()).unwrap_or("repo"));
    let mut name = base.clone();
    let mut n = 1;
    while sessions.iter().any(|s| s.session_name == name) {
        n += 1;
        name = format!("{base}-{n}");
    }
    let made = t
        .run(&[
            "new-session",
            "-d",
            "-s",
            &crate::tmux::new_name(&name),
            "-c",
            &crate::tmux::start_directory(&root.to_string_lossy()),
            "-P",
            "-F",
            "#{session_id}",
        ])
        .with_context(|| format!("creating the tmux session for '{}'", root.display()))?;
    Ok(made.trim().to_string())
}

fn branch_exists(repo_root: &Path, branch: &str, trust_repository: bool) -> bool {
    git_command(repo_root, trust_repository)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "--",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_git(repo_root: &Path, args: &[&str], trust_repository: bool) -> anyhow::Result<()> {
    let output = git_command(repo_root, trust_repository)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?} in {}", repo_root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {args:?} failed ({}): {}", output.status, stderr.trim());
    }
    Ok(())
}

/// A git call that failed: what it exited with, and what it said.
struct GitFailure {
    args: String,
    repo: PathBuf,
    /// `None` when git could not be run at all, and `stderr` says why.
    status: Option<std::process::ExitStatus>,
    stderr: String,
}

impl GitFailure {
    /// The message every failed git call here carries.
    fn message(&self) -> String {
        match self.status {
            Some(status) => format!("git {} failed ({status}): {}", self.args, self.stderr),
            None => format!(
                "running git {} in {}: {}",
                self.args,
                self.repo.display(),
                self.stderr
            ),
        }
    }

    fn into_error(self) -> anyhow::Error {
        anyhow::anyhow!(self.message())
    }
}

/// [`run_git`], keeping what git said apart from the message so it can be read.
fn git_capture(repo_root: &Path, args: &[&str], trust_repository: bool) -> Result<(), GitFailure> {
    let failure = |status, stderr| GitFailure {
        args: format!("{args:?}"),
        repo: repo_root.to_path_buf(),
        status,
        stderr,
    };
    let output = git_command(repo_root, trust_repository)
        .args(args)
        .output()
        .map_err(|e| failure(None, e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(failure(
        Some(output.status),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

// Follows herdr's src/worktree.rs (`is_dirty_worktree_remove_error`,
// `is_not_working_tree_remove_error`, `run_worktree_remove_command_with_recovery`,
// `leftover_worktree_checkout_matches_repo`, `git_common_worktrees_dir`,
// `worktree_list_contains_path`). Differences: the recovery is split from the git call, and
// reports the refusals a caller can ask a user about ([`RemoveRefusal`]) instead of a string;
// a folder git has no record of and that is not there either is a removal already done.
/// git's own words for a checkout a removal would lose work in, which only `--force`
/// removes: modified or untracked files, or submodules.
fn is_dirty_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("contains modified or untracked files")
        && lower.contains("use --force to delete it"))
        || lower.contains("working trees containing submodules cannot be moved or removed")
}

/// git's words for a path it does not know as a worktree.
fn is_not_working_tree_remove_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("is not a working tree") || lower.contains("is not a worktree")
}

/// What to do about a `git worktree remove` that failed: carry on when there is nothing left to
/// remove, clear the leftovers of an earlier removal when `force` allows, or fail. A failure a
/// force would cure is a [`WorktreeError::RemoveRefused`] when there was no force.
fn settle_failed_removal(
    record: &WorktreeRecord,
    force: bool,
    trust_repository: bool,
    failure: GitFailure,
) -> anyhow::Result<()> {
    let refused = |why: RemoveRefusal, failure: GitFailure| {
        anyhow::Error::new(WorktreeError::RemoveRefused {
            why,
            branch: record.branch.clone(),
            path: record.path.clone(),
            message: failure.message(),
        })
    };
    if is_not_working_tree_remove_error(&failure.stderr) {
        // git has let go of it: a removal that failed partway leaves the folder unregistered,
        // and one deleted by hand leaves nothing.
        match worktree_is_listed(&record.repo_root, &record.path, trust_repository) {
            Ok(false) => {}
            // Listed after all, or git cannot say: its own refusal stands.
            Ok(true) | Err(_) => return Err(failure.into_error()),
        }
        if !record.path.exists() {
            return Ok(());
        }
        if !leftover_matches_repo(&record.repo_root, &record.path, trust_repository) {
            return Err(failure.into_error());
        }
        if !force {
            return Err(refused(RemoveRefusal::Leftover, failure));
        }
        return std::fs::remove_dir_all(&record.path).map_err(|e| {
            anyhow::anyhow!(
                "{}; failed to remove leftover checkout {}: {e}",
                failure.message(),
                record.path.display()
            )
        });
    }
    if !force && is_dirty_remove_error(&failure.stderr) {
        let why = if failure.stderr.to_ascii_lowercase().contains("submodules") {
            RemoveRefusal::Submodules
        } else {
            RemoveRefusal::Dirty
        };
        return Err(refused(why, failure));
    }
    Err(failure.into_error())
}

/// Whether git lists `path` among the repository's worktrees.
fn worktree_is_listed(
    repo_root: &Path,
    path: &Path,
    trust_repository: bool,
) -> anyhow::Result<bool> {
    let output = git_command(repo_root, trust_repository)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("listing the repository's worktrees")?;
    anyhow::ensure!(
        output.status.success(),
        "git worktree list failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let wanted = canonical(path);
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| canonical(Path::new(listed)) == wanted))
}

/// Whether the folder at `path` was a checkout of this repository: its `.git` file points into
/// the repository's own `worktrees` directory, where git keeps what it knows of each one.
fn leftover_matches_repo(repo_root: &Path, path: &Path, trust_repository: bool) -> bool {
    let Ok(content) = std::fs::read_to_string(path.join(".git")) else {
        return false;
    };
    let Some(gitdir) = content.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.join(gitdir)
    };
    let Some(worktrees_dir) = git_common_worktrees_dir(repo_root, trust_repository) else {
        return false;
    };
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(&gitdir).starts_with(canonical(&worktrees_dir))
}

fn git_common_worktrees_dir(repo_root: &Path, trust_repository: bool) -> Option<PathBuf> {
    let output = git_command(repo_root, trust_repository)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let common_dir = stdout.trim();
    if common_dir.is_empty() {
        return None;
    }
    let common_dir = PathBuf::from(common_dir);
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        repo_root.join(common_dir)
    };
    Some(common_dir.join("worktrees"))
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_branch_replaces_slashes_with_dashes() {
        assert_eq!(sanitize_branch("feat/x"), "feat-x");
        assert_eq!(sanitize_branch("feat/nested/thing"), "feat-nested-thing");
        assert_eq!(sanitize_branch("main"), "main");
    }

    #[test]
    fn validate_branch_name_rejects_flags_blanks_and_hostile_names_but_keeps_ordinary_ones() {
        // The ordinary shapes a branch takes are all accepted.
        for ok in [
            "feat/x",
            "release/1.2",
            "fix-bug_3",
            "main",
            "user/2fa",
            "v1.2.3",
        ] {
            assert!(
                validate_branch_name(ok).is_ok(),
                "rejected a valid branch {ok:?}"
            );
        }

        // A leading dash is the bug: git reads it as one of its own options.
        for bad in ["-x", "-evil", "--base", "--force"] {
            assert!(
                matches!(
                    validate_branch_name(bad),
                    Err(WorktreeError::InvalidBranch(_))
                ),
                "accepted a dash-leading branch {bad:?}"
            );
        }

        // Blank, whitespace, and shell/git metacharacters are refused too.
        for bad in [
            "",
            "has space",
            "tab\there",
            "a;rm -rf",
            "$(x)",
            "a|b",
            "back`tick",
            "q?",
        ] {
            assert!(
                matches!(
                    validate_branch_name(bad),
                    Err(WorktreeError::InvalidBranch(_))
                ),
                "accepted a hostile branch {bad:?}"
            );
        }
    }

    #[test]
    fn the_computed_session_name_is_the_one_tmux_will_actually_create() {
        // Both halves of the name go through the same fold, so a dot-prefixed repo
        // (`~/.dotfiles`) and a dotted branch (`release/1.2`) are both accounted for.
        assert_eq!(
            crate::tmux::session_name(&format!("_dotfiles--{}", sanitize_branch("feat/x"))),
            "_dotfiles--feat-x"
        );
        assert_eq!(
            crate::tmux::session_name(&format!("repo--{}", sanitize_branch("release/1.2"))),
            "repo--release-1_2"
        );
        assert_eq!(
            crate::tmux::session_name(&format!(".dotfiles--{}", sanitize_branch("v2.0"))),
            "_dotfiles--v2_0"
        );
    }

    /// Mirrors herdr's own `trusted_repository_git_args_are_request_scoped`
    /// (`<herdr-src>/src/worktree.rs`): untrusted is just `-C <repo_root>`; trusted
    /// prepends `-c safe.directory=<repo_root>`.
    #[test]
    fn git_command_adds_safe_directory_only_when_trusted() {
        let args = |trust| {
            git_command(Path::new("/repo/taw"), trust)
                .get_args()
                .map(|a| a.to_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(args(false), ["-C", "/repo/taw"]);
        assert_eq!(
            args(true),
            ["-c", "safe.directory=/repo/taw", "-C", "/repo/taw"]
        );
    }

    #[test]
    fn a_record_of_another_machine_is_never_saved_and_an_older_file_reads_as_this_machines() {
        let mut other = record("rain", "w3", "studio:w7");
        other.machine = Some(0);
        let json = serde_json::to_value(&other).unwrap();
        assert!(json.get("machine").is_none(), "{json}");
        // A file written before the field existed, or by a taw that never had it.
        let back: WorktreeRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back.machine, None);
        assert_eq!(back.workspace_id, "studio:w7");
        // A file that does name one is read as this machine's: a machine is never read from disk.
        let mut named = serde_json::to_value(&other).unwrap();
        named["machine"] = serde_json::json!(2);
        let read: WorktreeRecord = serde_json::from_value(named).unwrap();
        assert_eq!(read.machine, None);
    }

    /// A worktree record cut from the session whose public id is `from`, in a repository at
    /// `/repo/app`.
    fn record(name: &str, from: &str, id: &str) -> WorktreeRecord {
        record_in(name, from, id, "/repo/app")
    }

    /// [`record`] in the repository at `root`: a worktree cut from another worktree is in
    /// the repository `git rev-parse --show-toplevel` finds from there, which is the first
    /// worktree's own checkout.
    fn record_in(name: &str, from: &str, id: &str, root: &str) -> WorktreeRecord {
        WorktreeRecord {
            workspace_id: id.to_string(),
            session_name: name.to_string(),
            path: PathBuf::from(format!("/wt/{name}")),
            branch: name.to_string(),
            base: None,
            repo_root: PathBuf::from(root),
            source_workspace_id: from.to_string(),
            created_unix_ms: 0,
            machine: None,
            real_path: None,
            real_repo_root: None,
        }
    }

    fn session<'a>(id: &'a str, name: &'a str, path: &'a str) -> SessionRef<'a> {
        SessionRef {
            id,
            name,
            path: Path::new(path),
        }
    }

    fn linked_names(linked: &[LinkedSession]) -> Vec<&str> {
        linked.iter().map(|l| l.session_name.as_str()).collect()
    }

    /// A machine's record whose repository root the feed resolved (`/var` is `/private/var` there) is
    /// followed from a session whose folder is given resolved, and none of it reaches `worktrees.json`.
    #[test]
    fn linked_among_follows_a_machines_session_by_the_resolved_repository_root() {
        let mut behind = record_in("app--a", "w1", "w5", "/var/x/app");
        behind.machine = Some(0);
        behind.real_repo_root = Some(PathBuf::from("/private/var/x/app"));
        let app = session("$1", "app", "/private/var/x/app");
        let child = session("$5", "app--a", "/private/var/x/app--a");

        let linked = linked_among(&[behind.clone()], &[app, child], app);
        assert_eq!(linked_names(&linked), ["app--a"]);

        // Without the resolved root the paths as written differ: today's behaviour, the control.
        behind.real_repo_root = None;
        assert!(linked_among(&[behind.clone()], &[app, child], app).is_empty());

        behind.real_path = Some(PathBuf::from("/private/var/x/app--a"));
        behind.real_repo_root = Some(PathBuf::from("/private/var/x/app"));
        let json = serde_json::to_string(&behind).unwrap();
        assert!(!json.contains("/private/var"), "{json}");
        assert!(!json.contains("real_"), "{json}");
    }

    /// A machine's folders are compared as the machine sent them, never resolved on this Mac: its repository
    /// root exists here behind a symlink and the session's folder does not, and the session is still linked.
    /// Mutation: `path_is_within` resolves the paths of a machine's record too (the root resolves to the far
    /// side of the link and the session folder does not, so it is no longer inside).
    #[test]
    fn linked_among_compares_a_machines_folders_as_it_sent_them() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("garden")).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let root = dir.path().join("link/garden");
        let folder = root.join("sub");
        assert!(!folder.exists(), "the session folder does not exist here");
        let mut record = record_in("app--a", "w1", "w5", root.to_str().unwrap());
        let app = session("$1", "app", folder.to_str().unwrap());
        let child = session("$5", "app--a", "/wt/app--a");

        record.machine = Some(0);
        let linked = linked_among(&[record], &[app, child], app);
        assert_eq!(linked_names(&linked), ["app--a"]);
    }

    /// This machine's own comparison keeps resolving: the root is the real folder (as git reports it) and the
    /// session's folder is the same place behind a symlink (as `/var` is `/private/var`), and it stays linked.
    /// Mutation: `path_is_within` never resolves (this machine's case fails as well).
    #[test]
    fn linked_among_still_resolves_this_machines_own_folders() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("garden")).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let root = real.join("garden");
        let folder = dir.path().join("link/garden");
        let record = record_in("app--a", "w1", "w5", root.to_str().unwrap());
        assert!(record.machine.is_none());
        let app = session("$1", "app", folder.to_str().unwrap());
        let child = session("$5", "app--a", "/wt/app--a");

        let linked = linked_among(&[record], &[app, child], app);
        assert_eq!(linked_names(&linked), ["app--a"]);
    }

    /// `linked_among` is [`linked_sessions`] on what the caller holds (the popup's `x` on a
    /// header reads it without a tmux of its own): the running worktree sessions cut from the
    /// session, nearest first, in the order the worktrees were made.
    #[test]
    fn linked_among_finds_the_running_sessions_cut_from_one_in_the_order_they_were_made() {
        let records = [
            record("app--a", "w1", "w5"),
            record("app--b", "w1", "w6"),
            record("other--x", "w2", "w7"),
        ];
        let app = session("$1", "app", "/repo/app");
        let sessions = [
            app,
            session("$5", "app--a", "/wt/app--a"),
            session("$6", "app--b", "/wt/app--b"),
            session("$2", "other", "/repo/other"),
            session("$7", "other--x", "/wt/other--x"),
        ];

        let linked = linked_among(&records, &sessions, app);

        assert_eq!(linked_names(&linked), ["app--a", "app--b"]);
        assert_eq!(linked[0].session_id, "$5");
        assert_eq!(linked[1].record.workspace_id, "w6");
    }

    #[test]
    fn a_worktree_session_is_no_primary_a_closed_one_is_not_counted_and_the_chain_goes_on() {
        let records = [
            record("app--a", "w1", "w5"),
            // Cut from the worktree `app--a`, whose session is closed.
            record_in("app--a--deep", "w5", "w6", "/wt/app--a"),
        ];
        let app = session("$1", "app", "/repo/app");
        let deep = session("$6", "app--a--deep", "/wt/app--a--deep");

        // `app--a` is closed, `app--a--deep` is running: still in the group of `app`.
        let linked = linked_among(&records, &[app, deep], app);
        assert_eq!(linked_names(&linked), ["app--a--deep"]);

        // A worktree has no group of its own, running or not.
        let a = session("$5", "app--a", "/wt/app--a");
        assert!(linked_among(&records, &[app, a, deep], a).is_empty());

        // Nothing running, nothing to strand.
        assert!(linked_among(&records, &[app], app).is_empty());
        assert!(linked_among(&[], &[app], app).is_empty());
    }

    /// tmux hands out ids afresh after a restart, so a record that names an id a session with
    /// nothing to do with its repository now has is not cut from that session.
    #[test]
    fn a_stale_id_does_not_link_a_session_outside_the_repository() {
        let records = [record("app--a", "w1", "w5")];
        let stranger = session("$1", "elsewhere", "/home/me/elsewhere");
        let a = session("$5", "app--a", "/wt/app--a");

        assert!(linked_among(&records, &[stranger, a], stranger).is_empty());
    }

    /// A hand-edited `worktrees.json` that loops back on itself cannot spin it: a record
    /// that names a session already followed is followed once.
    #[test]
    fn a_chain_that_loops_back_on_itself_ends() {
        let records = [
            record("app--a", "w1", "w5"),
            record_in("app--b", "w5", "w6", "/wt/app--a"),
            // The same session again, cut from `app--b`: a loop.
            record_in("app--a", "w6", "w5", "/wt/app--b"),
        ];
        let app = session("$1", "app", "/repo/app");
        let a = session("$5", "app--a", "/wt/app--a");
        let b = session("$6", "app--b", "/wt/app--b");

        let linked = linked_among(&records, &[app, a, b], app);

        assert_eq!(linked_names(&linked), ["app--a", "app--b"]);
    }

    /// A removal is told apart by git's English words, and git speaks the user's language
    /// unless asked not to: every call made for a repository asks for the C locale.
    #[test]
    fn git_speaks_english_to_every_worktree_call() {
        for trust in [false, true] {
            let command = git_command(Path::new("/repo/taw"), trust);
            let locale: Vec<_> = command
                .get_envs()
                .filter(|(name, _)| *name == "LC_ALL")
                .collect();
            assert_eq!(locale.len(), 1, "trust={trust}");
            assert_eq!(
                locale[0].1.and_then(|v| v.to_str()),
                Some("C"),
                "trust={trust}"
            );
        }
    }

    // Follows herdr's src/worktree.rs tests.
    #[test]
    fn dirty_remove_error_detection_matches_git_force_hint() {
        assert!(is_dirty_remove_error(
            "fatal: '/w/herdr' contains modified or untracked files, use --force to delete it"
        ));
        assert!(!is_dirty_remove_error(
            "fatal: '/w/herdr' is a missing but already registered worktree"
        ));
        assert!(!is_dirty_remove_error(
            "fatal: '/w/herdr' contains a locked worktree, use --force only if you know why"
        ));
    }

    #[test]
    fn submodule_remove_error_requires_force_confirmation() {
        assert!(is_dirty_remove_error(
            "fatal: working trees containing submodules cannot be moved or removed"
        ));
    }

    /// git says `is not a working tree` of a path it has no record of, and, in older
    /// versions, `is not a worktree`.
    #[test]
    fn a_path_git_does_not_know_is_told_from_one_it_refuses() {
        for said in [
            "fatal: '/w/herdr' is not a working tree",
            "fatal: '/w/herdr' IS NOT A WORKTREE",
        ] {
            assert!(is_not_working_tree_remove_error(said), "{said}");
            assert!(!is_dirty_remove_error(said), "{said}");
        }
        assert!(!is_not_working_tree_remove_error(
            "fatal: '/w/herdr' contains modified or untracked files, use --force to delete it"
        ));
    }

    /// `git_toplevel` cannot scope `safe.directory` to the repo root it is itself
    /// discovering (see its own comment), so a trusted call uses `*` instead of a
    /// specific path — and, either way, finds the same root.
    #[test]
    fn git_toplevel_trusts_every_repository_since_its_own_root_is_not_yet_known() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let status = crate::git::command()
            .arg("-C")
            .arg(&repo)
            .args(["init", "--quiet"])
            .status()
            .expect("run git init");
        assert!(status.success());

        let untrusted = git_toplevel(nested.to_str().unwrap(), false);
        let trusted = git_toplevel(nested.to_str().unwrap(), true);
        assert_eq!(
            untrusted, trusted,
            "trust must not change the discovered root"
        );
        assert_eq!(
            trusted.map(|p| std::fs::canonicalize(p).unwrap()),
            Some(std::fs::canonicalize(&repo).unwrap())
        );
    }
}
