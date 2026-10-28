//! `update`: fetch each installed GitHub plugin again at the ref it was installed with and
//! install what that gives in its place. A plugin linked from a local folder is left alone.
//!
//! Each commit gets a folder of its own, at its final path (see [`super::checkout`]): the new
//! commit is fetched into `<repo>@<12-char sha>` beside the current one, the trust preview
//! reads that folder's manifest and the build runs there, and only then do the registry's
//! records switch to it, in one atomic write; the old folder is removed after that, and a
//! failure to remove it is reported in the plugin's line and is not a failed update. A
//! decline, a failed fetch or a failed build removes the new folder and leaves the
//! live one and the records as they were. A folder left by Ctrl-C at the prompt, or by a run
//! that was killed (named for a commit no record points at), is removed at the next update or
//! install of that repository; records and the live folder are untouched. No
//! built tree is ever moved, so a build that writes its own absolute path keeps a path that
//! exists. An install made before this layout (a folder named `<repo>`) updates into it.
//!
//! The remote is asked for the ref's commit first (`git ls-remote`); when that is the commit
//! installed, nothing is fetched and the plugin is reported unchanged. A plugin pinned to a
//! full commit that is the one installed is unchanged without asking. A build only runs in a
//! folder this run made: when the commit's folder is a record's, the new folder is
//! `<repo>@<sha12>-<n>`.
//!
//! Plugins installed from subdirs of one repository share one folder, so they update
//! together: each one's gate and build run in the new folder, a decline or a failure by any of
//! them changes nothing, every record switches in the same write, and each is reported.
//!
//! Around the switch an enabled plugin gets what `plugin disable` then `plugin enable` give
//! it: both only change the registry's `enabled` flag. Neither stops anything (taw keeps no
//! plugin process to stop) and neither runs `[[startup]]` (herdr's plugins.mdx: startup
//! hooks do not run "when a plugin is linked or enabled"). So an update keeps the flag and
//! runs no startup hooks; the new copy's run at the next `taw plugin startup`.

use std::path::PathBuf;

use super::{PluginCtx, checkout, install, now_unix_ms, registry};

/// What an update did to one plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Installed a new commit in place of `from` (`None` when the record had none). `note`
    /// says what could not be tidied afterwards (the old folder would not go); the plugin
    /// works from its new folder either way.
    Updated {
        from: Option<String>,
        to: String,
        note: Option<String>,
    },
    /// The ref still points at the commit that is installed; nothing was built or changed.
    Unchanged { commit: String },
    /// Installed from a local folder with `plugin link`: its files are the user's own.
    Linked,
    /// The fetch, the consent or the build failed, for this plugin or for one sharing its
    /// checkout; the installed copy is as it was.
    Failed { code: &'static str, reason: String },
}

/// One plugin's line in the result of [`update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdate {
    pub id: String,
    pub outcome: Outcome,
}

/// Arguments to `update` that name no installed plugin, quoted and joined.
#[derive(Debug, thiserror::Error)]
#[error("plugin {0} not found")]
pub struct NotFound(pub String);

/// Updates the plugins named by `args`, or every registered plugin when `args` is empty. An
/// argument is a plugin id or the source it was installed from (`owner/repo[/subdir]`; see
/// [`select`]).
///
/// An argument that names nothing is an error naming it, raised before anything is touched.
/// After that no plugin's trouble stops the others: each one's line in the result says what
/// happened to it, and a plugin that failed is still installed as it was.
pub fn update(ctx: &PluginCtx, args: &[String], yes: bool) -> anyhow::Result<Vec<PluginUpdate>> {
    update_with(
        ctx,
        args,
        yes,
        &|owner, repo| format!("https://github.com/{owner}/{repo}.git"),
        &install::terminal_answer,
        &install::GitFetcher,
    )
}

/// The installed plugins `args` name, in the order given, each once; every installed plugin
/// when `args` is empty. An argument with a `/` is a source: `owner/repo` names every plugin
/// installed from that repository, `owner/repo/subdir` the one from that subdir (owner and
/// repo compared without case, as GitHub does). Any other argument is a plugin id. A source
/// with an `@ref` is refused: an update fetches the ref a plugin was installed with.
pub fn select(
    paths: &crate::paths::Paths,
    args: &[String],
) -> anyhow::Result<Vec<registry::PluginRecord>> {
    let installed = registry::load(paths)?.plugins;
    if args.is_empty() {
        return Ok(installed);
    }
    let mut chosen: Vec<registry::PluginRecord> = Vec::new();
    let mut unknown = Vec::new();
    for arg in args {
        let named: Vec<&registry::PluginRecord> = if arg.contains('/') {
            let spec = install::parse_spec(arg)?;
            anyhow::ensure!(
                spec.requested_ref.is_none(),
                "'{arg}': an update fetches the ref a plugin was installed with; name the \
                 source without @ref"
            );
            installed.iter().filter(|p| from_source(p, &spec)).collect()
        } else {
            installed.iter().filter(|p| &p.id == arg).collect()
        };
        if named.is_empty() {
            unknown.push(format!("'{arg}'"));
        }
        for record in named {
            if !chosen.iter().any(|c| c.id == record.id) {
                chosen.push(record.clone());
            }
        }
    }
    if !unknown.is_empty() {
        return Err(NotFound(unknown.join(", ")).into());
    }
    Ok(chosen)
}

fn from_source(record: &registry::PluginRecord, spec: &install::Spec) -> bool {
    matches!(
        &record.source,
        registry::PluginSource::Github { owner, repo, subdir, .. }
            if owner.eq_ignore_ascii_case(&spec.owner)
                && repo.eq_ignore_ascii_case(&spec.repo)
                && (spec.subdir.is_none() || *subdir == spec.subdir)
    )
}

/// [`update`] with the clone URL of `owner/repo` given by `url_for`, so a test can stand a
/// local repository in for GitHub, the consent question asked by `answer`, and the remote
/// read through `fetcher`, so a test can count its fetches.
pub(crate) fn update_with(
    ctx: &PluginCtx,
    args: &[String],
    yes: bool,
    url_for: &dyn Fn(&str, &str) -> String,
    answer: install::Answer,
    fetcher: &dyn install::Fetcher,
) -> anyhow::Result<Vec<PluginUpdate>> {
    let chosen = select(ctx.paths, args)?;
    let installed = registry::load(ctx.paths)?.plugins;
    let mut lines: Vec<PluginUpdate> = Vec::new();
    for record in &chosen {
        // Already updated with a plugin that shares its checkout.
        if lines.iter().any(|l| l.id == record.id) {
            continue;
        }
        let registry::PluginSource::Github { owner, repo, .. } = &record.source else {
            lines.push(PluginUpdate {
                id: record.id.clone(),
                outcome: Outcome::Linked,
            });
            continue;
        };
        // Every plugin that shares this checkout, the one asked for first.
        let mut group = vec![record.clone()];
        group.extend(
            installed
                .iter()
                .filter(|p| p.id != record.id && same_checkout(p, record))
                .cloned(),
        );
        let outcomes = update_checkout(
            ctx,
            (owner, repo),
            &url_for(owner, repo),
            &group,
            yes,
            answer,
            fetcher,
        );
        lines.extend(
            group
                .into_iter()
                .zip(outcomes)
                .map(|(r, outcome)| PluginUpdate { id: r.id, outcome }),
        );
    }
    Ok(lines)
}

/// Both records name the same folder, which is what makes two plugins of one repository
/// update together (a plugin reinstalled at another ref lives in a folder of its own).
fn same_checkout(a: &registry::PluginRecord, b: &registry::PluginRecord) -> bool {
    match (checkout::root_of(a), checkout::root_of(b)) {
        (Some(a), Some(b)) => checkout::same_folder(&a, &b),
        _ => false,
    }
}

/// The parts of a GitHub record an update reads (all `None` for a linked one).
struct Installed<'a> {
    subdir: Option<&'a str>,
    requested_ref: Option<&'a str>,
    commit: Option<&'a str>,
}

fn installed(record: &registry::PluginRecord) -> Installed<'_> {
    match &record.source {
        registry::PluginSource::Github {
            subdir,
            requested_ref,
            resolved_commit,
            ..
        } => Installed {
            subdir: subdir.as_deref(),
            requested_ref: requested_ref.as_deref(),
            commit: resolved_commit.as_deref(),
        },
        registry::PluginSource::Link => Installed {
            subdir: None,
            requested_ref: None,
            commit: None,
        },
    }
}

fn failed(err: &anyhow::Error) -> Outcome {
    Outcome::Failed {
        code: crate::cli::envelope::code_for(err),
        reason: format!("{err:#}"),
    }
}

/// Updates the plugins of `group`, which share a folder, to the commit their ref names now.
/// Returns one outcome per plugin, in `group`'s order.
fn update_checkout(
    ctx: &PluginCtx,
    (owner, repo): (&str, &str),
    url: &str,
    group: &[registry::PluginRecord],
    yes: bool,
    answer: install::Answer,
    fetcher: &dyn install::Fetcher,
) -> Vec<Outcome> {
    let all = |outcome: Outcome| vec![outcome; group.len()];
    let requested_ref = installed(&group[0]).requested_ref;
    if group
        .iter()
        .any(|r| installed(r).requested_ref != requested_ref)
    {
        let ids: Vec<String> = group.iter().map(|r| format!("'{}'", r.id)).collect();
        return all(failed(&anyhow::anyhow!(
            "plugins {} share one checkout but were installed at different refs; install \
             them again at one ref",
            ids.join(", ")
        )));
    }
    if let Err(err) = checkout::clear_leftovers(ctx.paths, owner, repo) {
        return all(failed(&err));
    }
    // Every plugin's files are where its record says, so a deleted folder is found missing
    // here and installed again, even at the same commit.
    let live = group.iter().all(|r| r.path.exists());
    let same_as = |commit: &str| live && group.iter().all(|r| installed(r).commit == Some(commit));
    // A plugin pinned to a full commit is unchanged when that is the commit installed: the
    // ref names it, so nothing is asked of the remote.
    if let Some(pinned) = requested_ref.filter(|r| install::is_full_sha(r))
        && same_as(&pinned.to_ascii_lowercase())
    {
        return all(Outcome::Unchanged {
            commit: pinned.to_ascii_lowercase(),
        });
    }
    // An error asking the remote is not the end: the fetch says why, if it fails too.
    let named = fetcher.ls_remote(url, requested_ref);
    if let Err(err) = install::refuse_short_sha(owner, repo, requested_ref, &named) {
        // An older taw let a short commit through as a ref; say how to get out of it.
        let err = match requested_ref {
            Some(r) => install::InstallError::InvalidSpec {
                spec: format!("{owner}/{repo}@{r}"),
                reason: format!(
                    "it was installed at a short commit, which cannot be updated; reinstall it \
                     with the full 40-character commit (`taw plugin install {owner}/{repo}@<the \
                     full 40-character commit>`), or at a branch or tag"
                ),
            }
            .into(),
            None => err,
        };
        return all(failed(&err));
    }
    let hint = named.unwrap_or(None);
    if let Some(commit) = hint.as_deref()
        && same_as(commit)
    {
        return all(Outcome::Unchanged {
            commit: commit.to_string(),
        });
    }
    let staged = match checkout::stage(
        ctx.paths,
        owner,
        repo,
        url,
        requested_ref,
        hint.as_deref(),
        fetcher,
    ) {
        Ok(staged) => staged,
        Err(err) => return all(failed(&err)),
    };
    if same_as(&staged.commit) {
        staged.discard();
        return all(Outcome::Unchanged {
            commit: staged.commit,
        });
    }
    if let Err((culprit, err)) =
        gate_and_build(ctx, &staged.dir, &staged.commit, group, yes, answer)
    {
        staged.discard();
        let sibling = Outcome::Failed {
            code: crate::cli::envelope::code_for(&err),
            reason: format!(
                "'{}' shares this plugin's checkout and failed: {err:#}",
                group[culprit].id
            ),
        };
        return (0..group.len())
            .map(|i| {
                if i == culprit {
                    failed(&err)
                } else {
                    sibling.clone()
                }
            })
            .collect();
    }
    if let Err(err) = record(ctx, &staged.dir, group, &staged.commit) {
        staged.discard();
        return all(failed(&err));
    }
    let old: Vec<PathBuf> = group.iter().filter_map(checkout::root_of).collect();
    let note = checkout::remove_unused(ctx.paths, &old, &staged.dir);
    group
        .iter()
        .map(|r| Outcome::Updated {
            from: installed(r).commit.map(str::to_string),
            to: staged.commit.clone(),
            note: note.clone(),
        })
        .collect()
}

/// Reads each plugin's manifest in the new folder (checking it still carries its id and
/// suits this taw), asks for consent for each, and runs each build there. The error names
/// the plugin (its index in `group`) whose step failed.
fn gate_and_build(
    ctx: &PluginCtx,
    dir: &std::path::Path,
    commit: &str,
    group: &[registry::PluginRecord],
    yes: bool,
    answer: install::Answer,
) -> Result<(), (usize, anyhow::Error)> {
    let mut members = Vec::new();
    for (i, record) in group.iter().enumerate() {
        let plugin_dir = install::plugin_dir(dir, installed(record).subdir);
        let manifest =
            checkout::load_member(ctx, &plugin_dir, Some(&record.id)).map_err(|e| (i, e))?;
        members.push((plugin_dir, manifest));
    }
    checkout::gate_and_build(&members, commit, yes, answer)
}

/// Points every plugin of `group` at its place in `dir` and records `commit`, each record
/// where it stands, in one atomic write of the registry.
fn record(
    ctx: &PluginCtx,
    dir: &std::path::Path,
    group: &[registry::PluginRecord],
    commit: &str,
) -> anyhow::Result<()> {
    let saved = registry::load(ctx.paths)?;
    let now = now_unix_ms();
    let mut moved = Vec::new();
    for r in group {
        let mut slot = saved
            .plugins
            .iter()
            .find(|p| p.id == r.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("plugin '{}' is no longer installed", r.id))?;
        if let registry::PluginSource::Github {
            subdir,
            resolved_commit,
            ..
        } = &mut slot.source
        {
            slot.path = install::plugin_dir(dir, subdir.as_deref());
            *resolved_commit = Some(commit.to_string());
        }
        slot.installed_unix_ms = now;
        moved.push(slot);
    }
    registry::add_all(ctx.paths, moved)
}

#[cfg(test)]
mod tests {
    use super::super::MANIFEST_FILE;
    use super::super::tests_support::test_paths;
    use super::*;
    use crate::paths::Paths;
    use std::cell::Cell;
    use std::path::Path;

    /// The real git, counting the two reads it is asked for.
    #[derive(Default)]
    struct Counting {
        ls_remotes: Cell<usize>,
        fetches: Cell<usize>,
    }

    use install::Fetcher;

    impl Fetcher for Counting {
        fn ls_remote(&self, url: &str, r: Option<&str>) -> anyhow::Result<Option<String>> {
            self.ls_remotes.set(self.ls_remotes.get() + 1);
            install::GitFetcher.ls_remote(url, r)
        }
        fn fetch(&self, dir: &Path, url: &str, r: Option<&str>) -> anyhow::Result<String> {
            self.fetches.set(self.fetches.get() + 1);
            install::GitFetcher.fetch(dir, url, r)
        }
    }

    fn git(args: &[&str]) {
        let status = crate::git::command().args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    fn git_capture(args: &[&str]) -> String {
        let output = crate::git::command().args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Nobody to ask: what a stdin that is not a terminal gives.
    fn nobody() -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Every file and folder under `dir`, `.git` and untracked build output included, with
    /// each file's size and a hash of its bytes, sorted.
    fn tree(dir: &Path) -> Vec<String> {
        use std::hash::{Hash as _, Hasher as _};
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(folder) = stack.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                let rel = path.strip_prefix(dir).unwrap().display().to_string();
                if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                    out.push(format!("{rel}/"));
                    stack.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    bytes.hash(&mut hasher);
                    out.push(format!("{rel} {} {:x}", bytes.len(), hasher.finish()));
                }
            }
        }
        out.sort();
        out
    }

    /// A local repository standing in for `github.com/<owner>/<repo>`.
    struct Remote {
        dir: tempfile::TempDir,
    }

    impl Remote {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            git(&["init", "-q", "-b", "main", dir.path().to_str().unwrap()]);
            Remote { dir }
        }
        fn path(&self) -> &str {
            self.dir.path().to_str().unwrap()
        }
        fn url(&self) -> String {
            format!("file://{}", self.path())
        }
        /// Writes a manifest (in `subdir`, or at the top) whose `[[build]]` is `build`, a
        /// shell line run in the plugin folder.
        fn write(&self, subdir: Option<&str>, id: &str, version: &str, build: &str) {
            let folder = match subdir {
                Some(s) => self.dir.path().join(s),
                None => self.dir.path().to_path_buf(),
            };
            std::fs::create_dir_all(&folder).unwrap();
            let manifest = format!(
                "id = \"{id}\"\nversion = \"{version}\"\nmin_herdr_version = \"0.7.0\"\n\n[[build]]\ncommand = [\"sh\", \"-c\", \"{build}\"]\n"
            );
            std::fs::write(folder.join(MANIFEST_FILE), manifest).unwrap();
        }
        /// Commits what was written; returns the new commit.
        fn commit_all(&self, message: &str) -> String {
            git(&["-C", self.path(), "add", "."]);
            git(&[
                "-C",
                self.path(),
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                message,
            ]);
            git_capture(&["-C", self.path(), "rev-parse", "HEAD"])
        }
        fn commit(&self, id: &str, version: &str, build: &str) -> String {
            self.write(None, id, version, build);
            self.commit_all(version)
        }
    }

    const BUILD_LOG: &str = "echo built >> build.log";

    struct Rig {
        dir: tempfile::TempDir,
        paths: Paths,
        cfg: crate::config::Config,
        tmux: crate::tmux::Tmux,
        taw_bin: std::path::PathBuf,
        fetcher: Counting,
    }

    impl Rig {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(dir.path());
            Rig {
                dir,
                paths,
                cfg: crate::config::Config::default(),
                tmux: crate::tmux::Tmux::with_socket_name("taw-plugin-update-test-unused"),
                taw_bin: std::path::PathBuf::from("/bin/true"),
                fetcher: Counting::default(),
            }
        }
        fn ctx(&self) -> PluginCtx<'_> {
            PluginCtx {
                tmux: &self.tmux,
                cfg: &self.cfg,
                paths: &self.paths,
                taw_bin: &self.taw_bin,
                server_env: None,
            }
        }
        /// Installs `remote` (its `subdir`) as `owner/repo` at `requested_ref`.
        fn try_install(
            &self,
            remote: &Remote,
            repo: &str,
            subdir: Option<&str>,
            requested_ref: Option<&str>,
        ) -> anyhow::Result<registry::PluginRecord> {
            self.try_install_as("owner", remote, repo, subdir, requested_ref)
        }
        fn try_install_as(
            &self,
            owner: &str,
            remote: &Remote,
            repo: &str,
            subdir: Option<&str>,
            requested_ref: Option<&str>,
        ) -> anyhow::Result<registry::PluginRecord> {
            let spec = install::Spec {
                owner: owner.to_string(),
                repo: repo.to_string(),
                subdir: subdir.map(str::to_string),
                requested_ref: requested_ref.map(str::to_string),
            };
            super::super::install_from_url(
                &self.ctx(),
                spec,
                &remote.url(),
                true,
                &nobody,
                &self.fetcher,
            )
        }
        fn install(&self, remote: &Remote, repo: &str, requested_ref: Option<&str>) {
            self.try_install(remote, repo, None, requested_ref).unwrap();
        }
        fn try_update(
            &self,
            remotes: &[(&str, &Remote)],
            args: &[&str],
            yes: bool,
            answer: install::Answer,
        ) -> anyhow::Result<Vec<PluginUpdate>> {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let urls: Vec<(String, String)> = remotes
                .iter()
                .map(|(repo, r)| (repo.to_string(), r.url()))
                .collect();
            update_with(
                &self.ctx(),
                &args,
                yes,
                &|_, repo| urls.iter().find(|(r, _)| r == repo).unwrap().1.clone(),
                answer,
                &self.fetcher,
            )
        }
        fn update(
            &self,
            remotes: &[(&str, &Remote)],
            ids: &[&str],
            yes: bool,
        ) -> Vec<PluginUpdate> {
            self.try_update(remotes, ids, yes, &nobody).unwrap()
        }
        /// The folder `repo`'s plugins live in now, as their record says.
        fn checkout(&self, repo: &str) -> std::path::PathBuf {
            registry::load(&self.paths)
                .unwrap()
                .plugins
                .iter()
                .filter(|p| {
                    matches!(&p.source, registry::PluginSource::Github { repo: r, .. } if r == repo)
                })
                .find_map(checkout::root_of)
                .unwrap_or_else(|| panic!("no record for {repo}"))
        }
        /// Every folder of `repo` under the managed folder, whatever its name.
        fn folders(&self, repo: &str) -> Vec<String> {
            let owner = install::checkout_dir(&self.paths, "owner", repo)
                .parent()
                .unwrap()
                .to_path_buf();
            let mut names: Vec<String> = std::fs::read_dir(owner)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n == repo || n.starts_with(&format!("{repo}@")))
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
        /// Installs each of `subdirs` of `remote`, then puts every one in the first one's
        /// folder: what installs made before a folder per commit left behind (a new install
        /// never builds in a folder a record points at, so it makes none of these).
        fn install_sharing_one_folder(&self, remote: &Remote, repo: &str, subdirs: &[&str]) {
            for subdir in subdirs {
                self.try_install(remote, repo, Some(subdir), None).unwrap();
            }
            let mut saved = registry::load(&self.paths).unwrap();
            let first = checkout::root_of(&saved.plugins[0]).unwrap();
            for record in saved.plugins.iter_mut().skip(1) {
                let own = checkout::root_of(record).unwrap();
                if let registry::PluginSource::Github { subdir, .. } = &record.source {
                    record.path = install::plugin_dir(&first, subdir.as_deref());
                }
                std::fs::remove_dir_all(own).unwrap();
            }
            registry::save(&self.paths, &saved).unwrap();
        }
        /// The folder the plugin `id` lives in now, as its record says.
        fn root_of(&self, id: &str) -> std::path::PathBuf {
            checkout::root_of(&registry::find(&self.paths, id).unwrap().unwrap()).unwrap()
        }
        fn builds(&self, repo: &str) -> usize {
            std::fs::read_to_string(self.checkout(repo).join("build.log"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }
        fn commit_of(&self, id: &str) -> Option<String> {
            match registry::find(&self.paths, id).unwrap().unwrap().source {
                registry::PluginSource::Github {
                    resolved_commit, ..
                } => resolved_commit,
                registry::PluginSource::Link => panic!("{id} is linked"),
            }
        }
        fn head(&self, repo: &str) -> String {
            git_capture(&[
                "-C",
                self.checkout(repo).to_str().unwrap(),
                "rev-parse",
                "HEAD",
            ])
        }
        fn ids(&self) -> Vec<String> {
            registry::load(&self.paths)
                .unwrap()
                .plugins
                .into_iter()
                .map(|p| p.id)
                .collect()
        }
        /// Nothing but the live folder is left of `repo`'s.
        fn assert_nothing_beside(&self, repo: &str) {
            let live = self.checkout(repo);
            assert_eq!(
                self.folders(repo),
                [live.file_name().unwrap().to_string_lossy().into_owned()],
                "a folder was left behind"
            );
        }
    }

    #[test]
    fn update_moves_a_branch_plugin_to_the_new_commit_and_runs_its_build() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let other = Remote::new();
        other.commit("beta", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        rig.install(&other, "beta", None);
        assert_eq!(rig.builds("alpha"), 1);

        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);
        let result = rig.update(&[("alpha", &remote)], &["alpha"], true);

        assert_eq!(
            result,
            vec![PluginUpdate {
                id: "alpha".to_string(),
                outcome: Outcome::Updated {
                    from: Some(first),
                    to: second.clone(),
                    note: None
                }
            }]
        );
        // The build ran once, in the copy that is now installed.
        assert_eq!(rig.builds("alpha"), 1, "the new commit's build must run");
        assert!(
            std::fs::read_to_string(rig.checkout("alpha").join(MANIFEST_FILE))
                .unwrap()
                .contains("2.0.0")
        );
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(second.as_str()));
        assert_eq!(rig.head("alpha"), second);
        assert_eq!(rig.ids(), ["alpha", "beta"], "the record keeps its place");
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn update_skips_a_plugin_whose_commit_is_the_one_installed_and_builds_nothing() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let before = tree(&rig.checkout("alpha"));

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert_eq!(
            result,
            vec![PluginUpdate {
                id: "alpha".to_string(),
                outcome: Outcome::Unchanged { commit: first }
            }]
        );
        assert_eq!(tree(&rig.checkout("alpha")), before);
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn update_leaves_a_tag_where_it_was_installed() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        git(&[
            "-C",
            remote.path(),
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "tag.gpgSign=false",
            "tag",
            "-a",
            "-m",
            "one",
            "v1",
        ]);
        let rig = Rig::new();
        rig.install(&remote, "alpha", Some("v1"));

        remote.commit("alpha", "2.0.0", BUILD_LOG);
        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert_eq!(
            result[0].outcome,
            Outcome::Unchanged {
                commit: first.clone()
            }
        );
        assert_eq!(rig.builds("alpha"), 1);
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(first.as_str()));
    }

    #[test]
    fn a_plugin_installed_at_a_commit_stays_there_and_is_unchanged_at_an_update() {
        // The fetch asks for the commit itself (an earlier install cloned `--branch <ref>`,
        // which refuses a sha), so a commit is a ref an install and an update both take.
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.try_install(&remote, "alpha", None, Some(&first))
            .unwrap();
        remote.commit("alpha", "2.0.0", BUILD_LOG);

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert_eq!(
            result[0].outcome,
            Outcome::Unchanged {
                commit: first.clone()
            }
        );
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(first.as_str()));
        assert_eq!(rig.builds("alpha"), 1);
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn a_reinstall_moves_the_plugins_that_share_its_folder_with_it() {
        let remote = Remote::new();
        remote.write(Some("one"), "one", "1.0.0", BUILD_LOG);
        remote.write(Some("two"), "two", "1.0.0", BUILD_LOG);
        remote.commit_all("first");
        let rig = Rig::new();
        rig.install_sharing_one_folder(&remote, "pair", &["one", "two"]);
        let old = rig.checkout("pair");
        remote.write(Some("one"), "one", "2.0.0", BUILD_LOG);
        remote.write(Some("two"), "two", "2.0.0", BUILD_LOG);
        let second = remote.commit_all("second");

        rig.try_install(&remote, "pair", Some("one"), None).unwrap();

        let live = rig.checkout("pair");
        assert_ne!(live, old);
        assert!(!old.exists());
        for sub in ["one", "two"] {
            let record = registry::find(&rig.paths, sub).unwrap().unwrap();
            assert_eq!(record.path, live.join(sub), "{sub} moved with its sibling");
            assert_eq!(rig.commit_of(sub).as_deref(), Some(second.as_str()));
            let log = live.join(sub).join("build.log");
            assert_eq!(std::fs::read_to_string(log).unwrap(), "built\n", "{sub}");
        }
        assert_eq!(rig.ids(), ["one", "two"]);
        rig.assert_nothing_beside("pair");
    }

    #[test]
    fn update_with_no_plugins_installed_lists_none() {
        let rig = Rig::new();
        assert_eq!(rig.update(&[], &[], true), vec![]);
    }

    #[test]
    fn update_reports_a_linked_plugin_and_touches_nothing_of_it() {
        let rig = Rig::new();
        let folder = rig.dir.path().join("mine");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(MANIFEST_FILE),
            "id = \"mine\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.7.0\"\n\n[[build]]\ncommand = [\"sh\", \"-c\", \"echo built >> build.log\"]\n",
        )
        .unwrap();
        super::super::link(&rig.ctx(), &folder, true).unwrap();
        let before = registry::load(&rig.paths).unwrap();

        let result = rig.update(&[], &[], true);

        assert_eq!(
            result,
            vec![PluginUpdate {
                id: "mine".to_string(),
                outcome: Outcome::Linked
            }]
        );
        assert!(
            !folder.join("build.log").exists(),
            "a linked plugin is never built"
        );
        assert_eq!(registry::load(&rig.paths).unwrap(), before);
    }

    #[test]
    fn a_failing_build_leaves_the_installed_copy_as_it_was_and_the_others_still_update() {
        let bad = Remote::new();
        let bad_first = bad.commit(
            "bad",
            "1.0.0",
            "echo built >> build.log; echo one > dist.txt",
        );
        let good = Remote::new();
        let good_first = good.commit("good", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&bad, "bad", None);
        rig.install(&good, "good", None);
        let before = tree(&rig.checkout("bad"));
        let record_before = registry::find(&rig.paths, "bad").unwrap();

        // The new build writes over its output, then fails.
        bad.commit("bad", "2.0.0", "echo broken > dist.txt; exit 3");
        let good_second = good.commit("good", "2.0.0", BUILD_LOG);
        let result = rig.update(&[("bad", &bad), ("good", &good)], &[], true);

        let by_id = |id: &str| result.iter().find(|r| r.id == id).unwrap().outcome.clone();
        let Outcome::Failed { reason, .. } = by_id("bad") else {
            panic!("bad should have failed: {result:?}");
        };
        assert!(reason.contains("build[0]"), "{reason}");
        assert_eq!(
            by_id("good"),
            Outcome::Updated {
                from: Some(good_first),
                to: good_second.clone(),
                note: None
            }
        );
        // The failed plugin's folder is byte for byte as it was, build output included.
        assert_eq!(tree(&rig.checkout("bad")), before);
        assert_eq!(
            std::fs::read_to_string(rig.checkout("bad").join("dist.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(registry::find(&rig.paths, "bad").unwrap(), record_before);
        assert_eq!(rig.commit_of("bad").as_deref(), Some(bad_first.as_str()));
        assert_eq!(rig.commit_of("good").as_deref(), Some(good_second.as_str()));
        rig.assert_nothing_beside("bad");
    }

    #[test]
    fn ids_select_which_plugins_update_and_an_unknown_id_changes_nothing() {
        let a = Remote::new();
        let a_first = a.commit("alpha", "1.0.0", BUILD_LOG);
        let b = Remote::new();
        b.commit("beta", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&a, "alpha", None);
        rig.install(&b, "beta", None);
        a.commit("alpha", "2.0.0", BUILD_LOG);
        let b_second = b.commit("beta", "2.0.0", BUILD_LOG);
        let remotes = [("alpha", &a), ("beta", &b)];

        let result = rig.update(&remotes, &["beta"], true);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "beta");
        assert_eq!(rig.commit_of("beta").as_deref(), Some(b_second.as_str()));
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(a_first.as_str()));

        // An unknown id among known ones: an error naming it, and nothing is updated.
        let err = rig
            .try_update(&remotes, &["alpha", "nope"], true, &nobody)
            .unwrap_err();
        assert!(err.downcast_ref::<NotFound>().is_some(), "{err}");
        assert!(err.to_string().contains("'nope'"), "{err}");
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(a_first.as_str()));
        assert_eq!(rig.builds("alpha"), 1);
    }

    #[test]
    fn a_source_names_the_plugins_installed_from_it() {
        let a = Remote::new();
        a.commit("alpha", "1.0.0", BUILD_LOG);
        let b = Remote::new();
        let b_first = b.commit("beta", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&a, "alpha", None);
        rig.install(&b, "beta", None);
        let a_second = a.commit("alpha", "2.0.0", BUILD_LOG);
        b.commit("beta", "2.0.0", BUILD_LOG);
        let remotes = [("alpha", &a), ("beta", &b)];

        // Owner and repo compared without case, as GitHub does.
        let result = rig.update(&remotes, &["Owner/ALPHA"], true);

        assert_eq!(
            result.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["alpha"]
        );
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(a_second.as_str()));
        assert_eq!(rig.commit_of("beta").as_deref(), Some(b_first.as_str()));

        // A source nothing was installed from is not found; one with a ref is refused.
        let err = rig
            .try_update(&remotes, &["owner/nope"], true, &nobody)
            .unwrap_err();
        assert!(err.to_string().contains("'owner/nope'"), "{err}");
        let err = rig
            .try_update(&remotes, &["owner/beta@v1"], true, &nobody)
            .unwrap_err();
        assert!(err.to_string().contains("without @ref"), "{err}");
        assert_eq!(rig.commit_of("beta").as_deref(), Some(b_first.as_str()));
    }

    #[test]
    fn a_declined_update_leaves_the_live_copy_and_the_record_as_they_were() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let before = tree(&rig.checkout("alpha"));
        let record_before = registry::find(&rig.paths, "alpha").unwrap();
        remote.commit("alpha", "2.0.0", BUILD_LOG);

        // Nobody to ask: refused as install refuses.
        let result = rig.update(&[("alpha", &remote)], &[], false);
        let Outcome::Failed { code, reason } = &result[0].outcome else {
            panic!("expected a refusal: {result:?}");
        };
        assert_eq!(*code, "trust_required");
        assert!(reason.contains("trust_required"), "{reason}");

        // Asked, and the answer is no.
        let asked = std::cell::Cell::new(0);
        let no = || {
            asked.set(asked.get() + 1);
            Ok(Some("n\n".to_string()))
        };
        let result = rig
            .try_update(&[("alpha", &remote)], &[], false, &no)
            .unwrap();
        assert_eq!(asked.get(), 1, "the question must be asked once");
        assert!(matches!(result[0].outcome, Outcome::Failed { .. }));

        assert_eq!(tree(&rig.checkout("alpha")), before);
        assert_eq!(registry::find(&rig.paths, "alpha").unwrap(), record_before);
        assert_eq!(rig.head("alpha"), first);
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn a_folder_left_by_a_killed_run_is_removed_at_the_next_update_and_the_next_install() {
        let remote = Remote::new();
        remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let live = rig.checkout("alpha");
        // What a run killed at the prompt leaves: a folder named for a commit that no
        // record points at, half fetched, and one still called pending.
        let owner_dir = live.parent().unwrap().to_path_buf();
        let leftovers = [
            owner_dir.join("alpha@0123456789ab"),
            owner_dir.join("alpha@pending"),
            owner_dir.join("alpha@0123456789ab-2"),
        ];
        for folder in &leftovers {
            git(&["init", "-q", folder.to_str().unwrap()]);
            std::fs::write(folder.join("junk"), "half a build").unwrap();
        }
        // A folder of another repository that merely starts alike stays.
        let other = owner_dir.join("alphabet@0123456789ab");
        std::fs::create_dir_all(&other).unwrap();
        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert!(
            matches!(&result[0].outcome, Outcome::Updated { to, .. } if *to == second),
            "{result:?}"
        );
        for folder in &leftovers {
            assert!(!folder.exists(), "{} was not removed", folder.display());
        }
        assert!(other.exists(), "another repository's folder was removed");
        rig.assert_nothing_beside("alpha");

        // The same at an install.
        let again = owner_dir.join("alpha@fedcba987654");
        std::fs::create_dir_all(&again).unwrap();
        rig.install(&remote, "alpha", None);
        assert!(!again.exists(), "{} was not removed", again.display());
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn a_deleted_checkout_is_installed_again() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        std::fs::remove_dir_all(rig.checkout("alpha")).unwrap();

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert_eq!(
            result[0].outcome,
            Outcome::Updated {
                from: Some(first.clone()),
                to: first.clone(),
                note: None
            }
        );
        assert!(rig.checkout("alpha").join(MANIFEST_FILE).exists());
        assert_eq!(rig.builds("alpha"), 1);
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(first.as_str()));
    }

    #[test]
    fn plugins_sharing_a_checkout_update_together_and_a_decline_by_one_changes_nothing() {
        let remote = Remote::new();
        remote.write(Some("one"), "one", "1.0.0", BUILD_LOG);
        remote.write(Some("two"), "two", "1.0.0", BUILD_LOG);
        let first = remote.commit_all("first");
        let rig = Rig::new();
        rig.install_sharing_one_folder(&remote, "pair", &["one", "two"]);
        remote.write(Some("one"), "one", "2.0.0", BUILD_LOG);
        remote.write(Some("two"), "two", "2.0.0", BUILD_LOG);
        let second = remote.commit_all("second");
        let before = tree(&rig.checkout("pair"));
        let records_before = registry::load(&rig.paths).unwrap();

        // Yes for the first, no for its sibling: nothing changes, and both say why.
        let answers = std::cell::RefCell::new(vec!["n\n", "y\n"]);
        let answer = || Ok(answers.borrow_mut().pop().map(str::to_string));
        let result = rig
            .try_update(&[("pair", &remote)], &["one"], false, &answer)
            .unwrap();
        assert_eq!(
            result.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        let Outcome::Failed { code, .. } = &result[1].outcome else {
            panic!("two declined: {result:?}");
        };
        assert_eq!(*code, "trust_required");
        let Outcome::Failed { reason, .. } = &result[0].outcome else {
            panic!("one is not updated without two: {result:?}");
        };
        assert!(reason.contains("'two'"), "{reason}");
        assert_eq!(tree(&rig.checkout("pair")), before);
        assert_eq!(registry::load(&rig.paths).unwrap(), records_before);

        // Consent given: both are built in the new copy and both records move.
        let result = rig.update(&[("pair", &remote)], &["one"], true);
        let updated = Outcome::Updated {
            from: Some(first.clone()),
            to: second.clone(),
            note: None,
        };
        assert_eq!(
            result,
            vec![
                PluginUpdate {
                    id: "one".to_string(),
                    outcome: updated.clone()
                },
                PluginUpdate {
                    id: "two".to_string(),
                    outcome: updated
                }
            ]
        );
        for sub in ["one", "two"] {
            let log = rig.checkout("pair").join(sub).join("build.log");
            assert_eq!(std::fs::read_to_string(log).unwrap(), "built\n", "{sub}");
            assert_eq!(rig.commit_of(sub).as_deref(), Some(second.as_str()));
        }
        assert_eq!(rig.ids(), ["one", "two"]);
        rig.assert_nothing_beside("pair");
    }

    #[test]
    fn a_failed_fetch_leaves_the_installed_copy_working_and_says_why() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let before = tree(&rig.checkout("alpha"));
        // The remote goes away.
        std::fs::remove_dir_all(remote.dir.path().join(".git")).unwrap();

        let result = rig.update(&[("alpha", &remote)], &[], true);

        let Outcome::Failed { reason, .. } = &result[0].outcome else {
            panic!("expected a failure: {result:?}");
        };
        assert!(reason.contains("git"), "{reason}");
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(first.as_str()));
        assert_eq!(tree(&rig.checkout("alpha")), before);
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn an_update_keeps_a_disabled_plugin_disabled() {
        let remote = Remote::new();
        remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        registry::set_enabled(&rig.paths, "alpha", false).unwrap();
        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);

        rig.update(&[("alpha", &remote)], &[], true);

        let record = registry::find(&rig.paths, "alpha").unwrap().unwrap();
        assert!(!record.enabled);
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(second.as_str()));
    }

    #[test]
    fn a_build_that_writes_its_own_path_still_finds_it_after_an_update() {
        let remote = Remote::new();
        // What a Python venv does: its scripts carry the absolute path of the folder it was
        // built in.
        let build = "pwd -P > built-in.txt";
        remote.commit("alpha", "1.0.0", build);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let second = remote.commit("alpha", "2.0.0", build);

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert!(
            matches!(&result[0].outcome, Outcome::Updated { to, .. } if *to == second),
            "{result:?}"
        );
        let live = rig.checkout("alpha");
        let written = std::fs::read_to_string(live.join("built-in.txt")).unwrap();
        let written = std::path::PathBuf::from(written.trim());
        assert!(
            written.join("built-in.txt").exists(),
            "the build wrote {}, which is gone",
            written.display()
        );
        assert_eq!(written, live.canonicalize().unwrap());
        let record = registry::find(&rig.paths, "alpha").unwrap().unwrap();
        assert_eq!(record.path.canonicalize().unwrap(), written);
        assert!(
            live.ends_with(format!("alpha@{}", &second[..12])),
            "{live:?}"
        );
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn a_reinstall_that_is_declined_changes_nothing_and_one_that_is_accepted_builds_beside_the_old()
    {
        let remote = Remote::new();
        remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let live = rig.checkout("alpha");
        let before = tree(&live);
        let records_before = registry::load(&rig.paths).unwrap();
        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);
        let spec = || install::Spec {
            owner: "owner".to_string(),
            repo: "alpha".to_string(),
            subdir: None,
            requested_ref: None,
        };

        // Nobody to ask, and an answer of n.
        for answer in [&nobody as install::Answer, &|| Ok(Some("n\n".to_string()))] {
            let err = super::super::install_from_url(
                &rig.ctx(),
                spec(),
                &remote.url(),
                false,
                answer,
                &rig.fetcher,
            )
            .unwrap_err();
            assert_eq!(
                err.downcast_ref::<install::InstallError>(),
                Some(&install::InstallError::TrustRequired)
            );
            assert_eq!(tree(&live), before);
            assert_eq!(registry::load(&rig.paths).unwrap(), records_before);
            rig.assert_nothing_beside("alpha");
        }
        assert_eq!(rig.builds("alpha"), 1, "no build ran for a decline");

        // Accepted: the new commit is built in a folder of its own, the records switch, and
        // the old folder goes.
        let record = super::super::install_from_url(
            &rig.ctx(),
            spec(),
            &remote.url(),
            true,
            &nobody,
            &rig.fetcher,
        )
        .unwrap();
        assert_ne!(rig.checkout("alpha"), live);
        assert!(!live.exists());
        assert_eq!(record.path, rig.checkout("alpha"));
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(second.as_str()));
        assert_eq!(rig.builds("alpha"), 1);
        assert_eq!(rig.ids(), ["alpha"]);
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn unchanged_is_found_without_a_download_and_a_moved_branch_is_fetched_once() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        assert_eq!(rig.fetcher.fetches.get(), 1, "the install fetches");

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert_eq!(
            result[0].outcome,
            Outcome::Unchanged {
                commit: first.clone()
            }
        );
        assert_eq!(
            rig.fetcher.fetches.get(),
            1,
            "an unchanged plugin must not be fetched"
        );
        assert_eq!(
            rig.fetcher.ls_remotes.get(),
            2,
            "the remote is asked each time"
        );

        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);
        let result = rig.update(&[("alpha", &remote)], &[], true);
        assert!(
            matches!(&result[0].outcome, Outcome::Updated { to, .. } if *to == second),
            "{result:?}"
        );
        assert_eq!(
            rig.fetcher.fetches.get(),
            2,
            "a moved branch is fetched once"
        );
    }

    #[test]
    fn an_install_made_before_a_folder_per_commit_updates_into_the_new_layout() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        // Put it where the older layout kept it: a folder named for the repository alone.
        let legacy = install::checkout_dir(&rig.paths, "owner", "alpha");
        std::fs::rename(rig.checkout("alpha"), &legacy).unwrap();
        let mut saved = registry::load(&rig.paths).unwrap();
        saved.plugins[0].path = legacy.clone();
        registry::save(&rig.paths, &saved).unwrap();
        assert_eq!(rig.checkout("alpha"), legacy);

        // The same commit is unchanged and stays where it is.
        let result = rig.update(&[("alpha", &remote)], &[], true);
        assert_eq!(
            result[0].outcome,
            Outcome::Unchanged {
                commit: first.clone()
            }
        );
        assert_eq!(rig.checkout("alpha"), legacy);

        // A new commit moves it into the new layout, and the old folder goes.
        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);
        let result = rig.update(&[("alpha", &remote)], &[], true);
        assert!(
            matches!(&result[0].outcome, Outcome::Updated { to, .. } if *to == second),
            "{result:?}"
        );
        assert!(!legacy.exists());
        let live = rig.checkout("alpha");
        assert!(
            live.ends_with(format!("alpha@{}", &second[..12])),
            "{live:?}"
        );
        assert!(live.join(MANIFEST_FILE).exists());
        rig.assert_nothing_beside("alpha");
        // And uninstall finds it through the record.
        super::super::uninstall(&rig.ctx(), "alpha").unwrap();
        assert!(!live.exists());
    }

    #[test]
    fn a_plugin_installed_under_another_case_keeps_its_folder_and_its_build() {
        // The temp folder is on this Mac's case-insensitive disk, where `github/Owner` and
        // `github/owner` are one folder, and GitHub ignores case, so people type either.
        // Comparing the two spellings as strings took the first plugin's live folder for a
        // leftover and removed it. On a disk that tells them apart there is nothing to prove,
        // so the test says so and returns.
        let remote = Remote::new();
        remote.write(Some("a"), "alpha", "1.0.0", BUILD_LOG);
        remote.write(Some("b"), "beta", "1.0.0", BUILD_LOG);
        remote.commit_all("both");
        let rig = Rig::new();
        // Only a disk that ignores case makes `Owner` and `owner` one folder.
        let probe = rig.paths.plugins_dir.join("github").join("Probe");
        std::fs::create_dir_all(&probe).unwrap();
        if !probe.with_file_name("probe").exists() {
            eprintln!(
                "skipped: the temp disk tells `Probe` from `probe`, so the two spellings are \
                 two folders here and there is nothing to prove"
            );
            return;
        }
        std::fs::remove_dir(&probe).unwrap();
        rig.try_install_as("Owner", &remote, "repo", Some("a"), None)
            .unwrap();
        let alpha = registry::find(&rig.paths, "alpha").unwrap().unwrap();
        assert!(alpha.path.join("build.log").exists());

        rig.try_install_as("owner", &remote, "repo", Some("b"), None)
            .unwrap();

        let beta = registry::find(&rig.paths, "beta").unwrap().unwrap();
        assert!(
            alpha.path.join("build.log").exists(),
            "alpha's folder or the file its build wrote was removed"
        );
        assert!(alpha.path.join(MANIFEST_FILE).exists());
        assert!(beta.path.join(MANIFEST_FILE).exists());
        assert!(!checkout::same_folder(
            &rig.root_of("alpha"),
            &rig.root_of("beta")
        ));
        assert_eq!(
            registry::find(&rig.paths, "alpha").unwrap().unwrap().path,
            alpha.path,
            "alpha's record moved"
        );
    }

    #[test]
    fn a_reinstall_typed_in_another_case_replaces_the_record_and_removes_the_old_folder() {
        let remote = Remote::new();
        remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.try_install_as("Owner", &remote, "alpha", None, None)
            .unwrap();
        let old = rig.root_of("alpha");

        rig.try_install_as("owner", &remote, "alpha", None, None)
            .unwrap();

        assert_eq!(rig.ids(), ["alpha"]);
        assert!(!old.exists(), "the old copy was left behind");
        assert!(rig.root_of("alpha").join(MANIFEST_FILE).exists());
    }

    /// A manifest whose build overwrites `dist.txt` and then exits 3 once `marker` exists.
    fn build_that_breaks_once(marker: &Path) -> String {
        format!(
            "if [ -e {} ]; then echo broken > dist.txt; exit 3; fi; echo good > dist.txt; echo built >> build.log",
            marker.display()
        )
    }

    #[test]
    fn a_reinstall_at_the_same_commit_whose_build_fails_changes_nothing() {
        let rig = Rig::new();
        let marker = rig.dir.path().join("marker");
        let remote = Remote::new();
        remote.commit("alpha", "1.0.0", &build_that_breaks_once(&marker));
        rig.install(&remote, "alpha", None);
        let live = rig.checkout("alpha");
        let before = tree(&live);
        let folders = rig.folders("alpha");
        let records = registry::load(&rig.paths).unwrap();
        let registry_bytes = std::fs::read(rig.paths.plugins_dir.join("registry.json")).unwrap();
        std::fs::write(&marker, "").unwrap();

        let err = rig.try_install(&remote, "alpha", None, None).unwrap_err();

        assert!(format!("{err:#}").contains("failed"), "{err:#}");
        assert_eq!(tree(&live), before, "the live folder was built in");
        assert_eq!(rig.folders("alpha"), folders);
        assert_eq!(registry::load(&rig.paths).unwrap(), records);
        assert_eq!(
            std::fs::read(rig.paths.plugins_dir.join("registry.json")).unwrap(),
            registry_bytes
        );
    }

    #[test]
    fn a_reinstall_at_the_same_commit_builds_in_a_folder_of_its_own_and_the_old_one_goes() {
        let rig = Rig::new();
        let marker = rig.dir.path().join("marker");
        let remote = Remote::new();
        let commit = remote.commit("alpha", "1.0.0", &build_that_breaks_once(&marker));
        rig.install(&remote, "alpha", None);
        let live = rig.checkout("alpha");

        let record = rig.try_install(&remote, "alpha", None, None).unwrap();

        let now = rig.checkout("alpha");
        assert_ne!(now, live);
        assert!(
            now.ends_with(format!("alpha@{}-2", &commit[..12])),
            "{now:?}"
        );
        assert_eq!(record.path, now);
        assert!(!live.exists(), "the old folder was left behind");
        assert_eq!(rig.builds("alpha"), 1, "the build ran in the new folder");
        assert_eq!(
            std::fs::read_to_string(now.join("dist.txt"))
                .unwrap()
                .trim(),
            "good"
        );
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(commit.as_str()));
        rig.assert_nothing_beside("alpha");

        // And once more: the smallest free number from 2 up, which is 2 again.
        let again = rig.try_install(&remote, "alpha", None, None).unwrap();
        assert!(
            again.path.ends_with(format!("alpha@{}", &commit[..12])),
            "{:?}",
            again.path
        );
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn an_update_to_a_commit_whose_folder_is_another_records_builds_in_a_folder_of_its_own() {
        let remote = Remote::new();
        remote.write(Some("a"), "alpha", "1.0.0", BUILD_LOG);
        remote.commit_all("first");
        let rig = Rig::new();
        rig.try_install(&remote, "repo", Some("a"), None).unwrap();
        // A second plugin of the repository, installed at the commit alpha will update to.
        remote.write(Some("b"), "beta", "1.0.0", BUILD_LOG);
        let second = remote.commit_all("second");
        rig.try_install(&remote, "repo", Some("b"), None).unwrap();
        let beta_root = rig.root_of("beta");
        let beta_before = tree(&beta_root);
        let alpha_old = rig.root_of("alpha");
        assert_ne!(alpha_old, beta_root);

        let result = rig.update(&[("repo", &remote)], &["alpha"], true);

        assert!(
            matches!(&result[0].outcome, Outcome::Updated { to, .. } if *to == second),
            "{result:?}"
        );
        assert_eq!(tree(&beta_root), beta_before, "beta's folder was built in");
        let alpha_new = rig.root_of("alpha");
        assert!(
            alpha_new.ends_with(format!("repo@{}-2", &second[..12])),
            "{alpha_new:?}"
        );
        assert!(
            registry::find(&rig.paths, "alpha")
                .unwrap()
                .unwrap()
                .path
                .join("build.log")
                .exists()
        );
        assert!(!alpha_old.exists());
        assert!(rig.root_of("beta").join("b").join(MANIFEST_FILE).exists());
    }

    #[test]
    fn a_reinstall_whose_plain_and_second_folders_both_belong_to_records_builds_on_the_third() {
        let remote = Remote::new();
        remote.write(Some("a"), "alpha", "1.0.0", BUILD_LOG);
        remote.write(Some("b"), "beta", "1.0.0", BUILD_LOG);
        let commit = remote.commit_all("both");
        let rig = Rig::new();
        rig.try_install(&remote, "repo", Some("a"), None).unwrap();
        rig.try_install(&remote, "repo", Some("b"), None).unwrap();
        let (plain, second) = (rig.root_of("alpha"), rig.root_of("beta"));
        assert!(
            plain.ends_with(format!("repo@{}", &commit[..12])),
            "{plain:?}"
        );
        assert!(
            second.ends_with(format!("repo@{}-2", &commit[..12])),
            "{second:?}"
        );
        let beta_tree = tree(&second);
        let beta_record = registry::find(&rig.paths, "beta").unwrap().unwrap();

        rig.try_install(&remote, "repo", Some("a"), None).unwrap();

        let third = rig.root_of("alpha");
        assert!(
            third.ends_with(format!("repo@{}-3", &commit[..12])),
            "{third:?}"
        );
        assert!(third.join("a").join(MANIFEST_FILE).exists());
        assert_eq!(tree(&second), beta_tree, "beta's folder was touched");
        assert_eq!(
            registry::find(&rig.paths, "beta").unwrap().unwrap(),
            beta_record
        );
        assert_eq!(rig.root_of("beta"), second);
    }

    /// A fetcher that writes into the folder it is given and then fails.
    struct Failing;

    impl Fetcher for Failing {
        fn ls_remote(&self, _: &str, _: Option<&str>) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn fetch(&self, dir: &Path, _: &str, _: Option<&str>) -> anyhow::Result<String> {
            std::fs::write(dir.join("partial"), "half a fetch").unwrap();
            anyhow::bail!("the network went away")
        }
    }

    #[test]
    fn a_fetch_that_fails_leaves_a_folder_that_was_already_there_and_removes_only_its_own() {
        let rig = Rig::new();
        let hint = "0123456789abcdef0123456789abcdef01234567";
        let existing = install::checkout_dir(&rig.paths, "owner", "repo")
            .with_file_name(format!("repo@{}", &hint[..12]));
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("keep"), "a record's file").unwrap();
        let before = tree(&existing);

        let err = checkout::stage(
            &rig.paths,
            "owner",
            "repo",
            "file:///nowhere",
            None,
            Some(hint),
            &Failing,
        )
        .err()
        .expect("the fetch fails");

        assert!(format!("{err:#}").contains("network went away"), "{err:#}");
        assert_eq!(tree(&existing), before, "the folder that was there changed");
        assert_eq!(
            rig.folders("repo"),
            [format!("repo@{}", &hint[..12])],
            "the failed run left its own folder behind"
        );
    }

    #[test]
    fn an_update_of_a_plugin_recorded_at_a_short_commit_is_refused_and_says_to_reinstall() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        // What an older taw recorded: the short commit as the ref.
        let mut saved = registry::load(&rig.paths).unwrap();
        if let registry::PluginSource::Github { requested_ref, .. } = &mut saved.plugins[0].source {
            *requested_ref = Some(first[..7].to_string());
        }
        registry::save(&rig.paths, &saved).unwrap();
        let fetches = rig.fetcher.fetches.get();
        let live = rig.checkout("alpha");
        let before = tree(&live);

        let result = rig.update(&[("alpha", &remote)], &[], true);

        let Outcome::Failed { reason, .. } = &result[0].outcome else {
            panic!("a short commit is refused: {result:?}");
        };
        assert!(reason.contains("full 40-character commit"), "{reason}");
        assert!(
            reason.contains("taw plugin install owner/alpha@"),
            "{reason}"
        );
        assert_eq!(rig.fetcher.fetches.get(), fetches, "something was fetched");
        assert_eq!(tree(&live), before);
    }

    #[test]
    fn a_ref_that_looks_like_a_short_commit_and_that_the_remote_does_not_name_is_refused() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();

        let err = rig
            .try_install(&remote, "alpha", None, Some(&first[..7]))
            .unwrap_err();

        let text = format!("{err:#}");
        assert!(
            text.contains("give the full 40-character commit, or a branch or tag"),
            "{text}"
        );
        assert_eq!(rig.fetcher.fetches.get(), 0, "something was fetched");
        assert!(rig.folders("alpha").is_empty());
        assert!(rig.ids().is_empty());

        // A branch that merely looks like a short commit is a name the remote knows.
        git(&["-C", remote.path(), "branch", "deadbeef"]);
        rig.try_install(&remote, "alpha", None, Some("deadbeef"))
            .unwrap();
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(first.as_str()));
    }

    #[test]
    fn an_update_of_a_plugin_pinned_to_its_full_commit_is_unchanged_with_nothing_asked() {
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.try_install(&remote, "alpha", None, Some(&first))
            .unwrap();
        remote.commit("alpha", "2.0.0", BUILD_LOG);
        let (asked, fetched) = (rig.fetcher.ls_remotes.get(), rig.fetcher.fetches.get());
        assert_eq!(fetched, 1);

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert_eq!(
            result[0].outcome,
            Outcome::Unchanged {
                commit: first.clone()
            }
        );
        assert_eq!(rig.fetcher.fetches.get(), fetched, "an update fetched");
        assert_eq!(rig.fetcher.ls_remotes.get(), asked, "an update asked");
        rig.assert_nothing_beside("alpha");
    }

    #[test]
    fn a_legacy_folder_that_no_record_points_at_is_cleared_with_the_leftovers() {
        let remote = Remote::new();
        remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let live = rig.checkout("alpha");
        // A folder named for the repository alone, as installs before a folder per commit
        // made, which a record no longer names.
        let legacy = install::checkout_dir(&rig.paths, "owner", "alpha");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("junk"), "old copy").unwrap();
        // Another repository's folder that merely starts alike stays.
        let other = install::checkout_dir(&rig.paths, "owner", "alphabet");
        std::fs::create_dir_all(&other).unwrap();

        let result = rig.update(&[("alpha", &remote)], &[], true);

        assert!(matches!(result[0].outcome, Outcome::Unchanged { .. }));
        assert!(!legacy.exists(), "the legacy folder was left");
        assert!(other.exists(), "another repository's folder was removed");
        assert!(live.join(MANIFEST_FILE).exists(), "the live folder went");
    }

    #[test]
    fn a_failure_to_remove_the_old_folder_is_in_the_line_and_the_update_stands() {
        use std::os::unix::fs::PermissionsExt as _;
        if crate::test_root::skip_as_root("root empties a folder of mode 0500") {
            return;
        }
        let remote = Remote::new();
        let first = remote.commit("alpha", "1.0.0", BUILD_LOG);
        let rig = Rig::new();
        rig.install(&remote, "alpha", None);
        let old = rig.checkout("alpha");
        // A folder that cannot be emptied.
        let stuck = old.join("stuck");
        std::fs::create_dir(&stuck).unwrap();
        std::fs::write(stuck.join("file"), "x").unwrap();
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
        let second = remote.commit("alpha", "2.0.0", BUILD_LOG);

        let result = rig.update(&[("alpha", &remote)], &[], true);

        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).unwrap();
        let Outcome::Updated { from, to, note } = &result[0].outcome else {
            panic!("a failure to tidy is not a failed update: {result:?}");
        };
        assert_eq!(
            (from.as_deref(), to.as_str()),
            (Some(first.as_str()), second.as_str())
        );
        assert!(
            note.as_deref()
                .is_some_and(|n| n.contains(old.to_str().unwrap())),
            "{note:?}"
        );
        assert_eq!(rig.commit_of("alpha").as_deref(), Some(second.as_str()));
        assert_ne!(rig.checkout("alpha"), old);
        assert!(rig.checkout("alpha").join(MANIFEST_FILE).exists());
    }
}
