//! Where a GitHub plugin's files live: a folder per commit, at its final path.
//!
//! herdr installs each commit of a plugin into a folder of its own (a numbered one under
//! `plugins/github-installations/<id>/`) and never moves a built tree. taw does the same, with
//! the commit in the name: `<state>/plugins/github/<owner>/<repo>@<12-char sha>`. A build that
//! writes an absolute path (a Python venv's scripts) then keeps pointing at a folder that is
//! still there after the install or update.
//!
//! The new commit is fetched into a folder this run creates, the trust preview, the gate and
//! the build read and run there, and only then do the registry's records switch to it, in one
//! atomic write; the old folder is removed after that. A build never runs in a folder a record
//! points at: a reinstall at the commit already installed, or an update to a commit whose
//! folder another record lives in, fetches into `<repo>@<sha12>-<n>` (the smallest `n` from 2
//! up that is free). Everything that finds a plugin's files reads the record's `path`, so an
//! install made before this layout (a folder named `<repo>`) is found the same way and moves
//! to the new layout at its next update.
//!
//! Whether a folder is the one a record points at is decided by file identity (device and
//! inode), never by comparing path strings: the disk may compare names without case, so
//! `Owner/repo` and `owner/repo` are one folder.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use super::install::{self, Answer, Fetcher};
use super::manifest::{self, PluginManifest};
use super::{MANIFEST_FILE, PluginCtx, registry};
use crate::paths::Paths;

/// `<state>/plugins/github/<owner>/<repo>@<12-char sha>`: the folder of one commit.
pub fn commit_dir(paths: &Paths, owner: &str, repo: &str, commit: &str) -> PathBuf {
    let short: String = commit.chars().take(12).collect();
    named(paths, owner, repo, &short)
}

fn named(paths: &Paths, owner: &str, repo: &str, tag: &str) -> PathBuf {
    install::checkout_dir(paths, owner, repo).with_file_name(format!("{repo}@{tag}"))
}

/// The folder a GitHub plugin's record puts it in: its `path` without the plugin's `subdir`.
/// `None` for a linked plugin, and for a record whose path does not end in its subdir.
pub fn root_of(record: &registry::PluginRecord) -> Option<PathBuf> {
    let registry::PluginSource::Github { subdir, .. } = &record.source else {
        return None;
    };
    let mut root = record.path.clone();
    if let Some(subdir) = subdir {
        if !record.path.ends_with(subdir) {
            return None;
        }
        for _ in Path::new(subdir).components() {
            root = root.parent()?.to_path_buf();
        }
    }
    Some(root)
}

/// The path of every record, linked or installed.
fn record_paths(paths: &Paths) -> anyhow::Result<Vec<PathBuf>> {
    Ok(registry::load(paths)?
        .plugins
        .into_iter()
        .map(|p| p.path)
        .collect())
}

/// Whether `a` and `b` are the same folder on disk, by device and inode. Two folders that do
/// not both exist have no identity to compare, so they are the same only when their paths are.
pub fn same_folder(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => a == b,
    }
}

/// Whether one of `record_paths` is `dir` or lies inside it, by file identity: a record's
/// folder is never taken for unused because its path was typed in another case.
fn in_use(record_paths: &[PathBuf], dir: &Path) -> bool {
    let Ok(target) = std::fs::metadata(dir) else {
        return false;
    };
    record_paths.iter().any(|path| {
        path.ancestors().any(|ancestor| {
            std::fs::metadata(ancestor)
                .is_ok_and(|m| m.dev() == target.dev() && m.ino() == target.ino())
        })
    })
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Removes the folders of `owner/repo` that no record points at: what a run that was killed
/// between its fetch and the switch of the records leaves behind (`<repo>@<sha>`,
/// `<repo>@<sha>-<n>`, `<repo>@pending`), and a folder named `<repo>` alone, which is how
/// installs made before a folder per commit kept their files. Called at the start of every
/// install and update of a plugin of that repository.
pub fn clear_leftovers(paths: &Paths, owner: &str, repo: &str) -> anyhow::Result<()> {
    let Some(parent) = install::checkout_dir(paths, owner, repo)
        .parent()
        .map(Path::to_path_buf)
    else {
        return Ok(());
    };
    let entries = match std::fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", parent.display())),
    };
    let pointed = record_paths(paths)?;
    let stem = format!("{}@", repo.to_ascii_lowercase());
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if (name.starts_with(&stem) || name == repo.to_ascii_lowercase())
            && path.is_dir()
            && !in_use(&pointed, &path)
        {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing the leftover {}", path.display()))?;
        }
    }
    Ok(())
}

/// A commit's folder, fetched by this run.
pub struct Staged {
    pub dir: PathBuf,
    pub commit: String,
}

impl Staged {
    /// Removes the folder this run made.
    pub fn discard(&self) {
        if self.dir.exists()
            && let Err(err) = std::fs::remove_dir_all(&self.dir)
        {
            log::warn!("plugin: could not remove {}: {err}", self.dir.display());
        }
    }
}

/// `<repo>@<tag>` when nothing is there, else `<repo>@<tag>-<n>` for the smallest `n` from 2
/// up with nothing there.
fn free_name(paths: &Paths, owner: &str, repo: &str, tag: &str) -> PathBuf {
    let taken = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    let first = named(paths, owner, repo, tag);
    if !taken(&first) {
        return first;
    }
    (2..)
        .map(|n| named(paths, owner, repo, &format!("{tag}-{n}")))
        .find(|path| !taken(path))
        .unwrap_or(first)
}

/// Makes the folder `free_name` picks, with a create that fails when the folder exists, and
/// returns it: the folder is this run's, so the error path may remove it without ever
/// touching one a record points at (a name taken between the pick and the create is skipped).
fn claim(paths: &Paths, owner: &str, repo: &str, tag: &str) -> anyhow::Result<PathBuf> {
    for _ in 0..64 {
        let candidate = free_name(paths, owner, repo, tag);
        if let Some(parent) = candidate.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    }
    anyhow::bail!("no free folder name for {repo}@{tag}")
}

/// Fetches the commit `requested_ref` names into a folder this run creates. `hint` is the
/// commit the remote named when asked (see [`Fetcher::ls_remote`]), which names the folder
/// before the fetch; when the fetch gives another commit, the folder (nothing is built in it
/// yet) is renamed to that one's. A name already taken (the commit is installed, or a record
/// lives in that folder) is never reused: the folder gets the next `-<n>` name, so a build
/// only ever runs where this run made the folder.
pub fn stage(
    paths: &Paths,
    owner: &str,
    repo: &str,
    url: &str,
    requested_ref: Option<&str>,
    hint: Option<&str>,
    fetcher: &dyn Fetcher,
) -> anyhow::Result<Staged> {
    let short = |commit: &str| commit.chars().take(12).collect::<String>();
    let tag = hint.map_or_else(|| "pending".to_string(), short);
    let target = claim(paths, owner, repo, &tag)?;
    let commit = match fetcher.fetch(&target, url, requested_ref) {
        Ok(commit) => commit,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&target);
            return Err(err);
        }
    };
    if short(&commit) == tag {
        return Ok(Staged {
            dir: target,
            commit,
        });
    }
    let actual = free_name(paths, owner, repo, &short(&commit));
    if let Err(err) = std::fs::rename(&target, &actual) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(err)
            .with_context(|| format!("moving {} to {}", target.display(), actual.display()));
    }
    Ok(Staged {
        dir: actual,
        commit,
    })
}

/// Reads the manifest of the plugin whose folder is `dir`, checks it still carries the id
/// `expect_id` (when one is given) and suits this taw.
pub fn load_member(
    ctx: &PluginCtx,
    dir: &Path,
    expect_id: Option<&str>,
) -> anyhow::Result<PluginManifest> {
    let path = dir.join(MANIFEST_FILE);
    let manifest = manifest::load(&path)
        .map_err(|e| anyhow::anyhow!("plugin manifest at {}: {e:#}", path.display()))?;
    if let Some(id) = expect_id {
        anyhow::ensure!(
            manifest.id == id,
            "plugin '{id}' now calls itself '{}' at this commit; install it again by hand to \
             take the new id",
            manifest.id
        );
    }
    install::check_min_version(ctx.cfg, &manifest)?;
    Ok(manifest)
}

/// Shows each plugin's trust preview (naming `commit`, the one fetched) and asks for consent;
/// then, all consent given, runs each build in its folder. The error names the plugin (its
/// index in `members`) whose step failed.
pub fn gate_and_build(
    members: &[(PathBuf, PluginManifest)],
    commit: &str,
    yes: bool,
    answer: Answer,
) -> Result<(), (usize, anyhow::Error)> {
    for (i, (_, manifest)) in members.iter().enumerate() {
        install::gate_with(manifest, Some(commit), yes, answer).map_err(|e| (i, e))?;
    }
    for (i, (dir, manifest)) in members.iter().enumerate() {
        install::run_build_commands(manifest, dir).map_err(|e| (i, e))?;
    }
    Ok(())
}

/// Removes each of `old` that no record points at now (and that is not `keep`), after the
/// records switched. A failure is not a failed install: the plugin works from its new folder.
/// Returns what went wrong, for the line the update prints.
pub fn remove_unused(paths: &Paths, old: &[PathBuf], keep: &Path) -> Option<String> {
    let pointed = match record_paths(paths) {
        Ok(pointed) => pointed,
        Err(err) => {
            return Some(format!(
                "could not tell which old copies are unused: {err:#}"
            ));
        }
    };
    let github = paths.plugins_dir.join("github");
    let mut notes = Vec::new();
    let mut seen: Vec<&PathBuf> = Vec::new();
    for dir in old {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir);
        if same_folder(dir, keep)
            || in_use(&pointed, dir)
            || !dir.exists()
            || !dir.starts_with(&github)
            || has_parent_component(dir)
        {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(dir) {
            log::warn!("plugin: could not remove {}: {err}", dir.display());
            notes.push(format!(
                "could not remove the old copy {}: {err}",
                dir.display()
            ));
        }
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}
