//! The create hooks, against a real (throwaway) tmux server.
//!
//! `[sessions] pre_create_hook` and `post_create_hook` run the way tmux-session-wizard runs
//! its own two hooks. The value is a command line (a path, or a path with flags of its
//! own), and the session name and the directory follow it as two separate arguments that no
//! shell ever reads again. The pre hook runs every time a directory is opened, before taw
//! looks for a running session, and may answer with a name and a directory; a non-zero exit
//! stops the open. The post hook runs only when a session was just created, after it exists
//! and before anyone is switched to it, and nothing it does changes the outcome. A hook that
//! runs longer than `[sessions] hook_timeout_seconds` is stopped: the pre hook's open is
//! refused, and the post hook's is logged.
//!
//! Every test runs on its own `-L` socket and tears it down on drop; the developer's own
//! tmux server is never addressed. A hook that calls `tmux` itself is handed the throwaway
//! server in `$TMUX`, since a plain `tmux` would otherwise reach whichever server this
//! suite was started inside.
//!
//! The config reaches `open_or_create` the way a user's file does, through TOML, so a hook
//! value is checked as written and not as a value some other code built.

mod common;

use common::shell_env::ShellHome;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use taw::config::Config;
use taw::session::create::{Outcome, open_or_create};
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    /// One session, `main`, running in `at`.
    fn start(at: &Path) -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = format!("taw-test-hooks-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&socket);
        let home = ShellHome::new();
        // `-f /dev/null` so the host's own tmux.conf cannot change what a test observes,
        // and `/bin/sh` so no login shell rc file runs in a pane.
        home.start_server(
            &t,
            &[
                "-d",
                "-s",
                "main",
                "-c",
                at.to_str().unwrap(),
                "-x",
                "80",
                "-y",
                "24",
                "/bin/sh",
            ],
        );
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        Server(t, home)
    }

    /// `TMUX=<this server's socket>,0,0`, to put in front of a hook value: a hook that runs
    /// `tmux` then reaches this server and nothing else.
    fn tmux_env(&self) -> String {
        let socket = self
            .0
            .run(&["display-message", "-p", "#{socket_path}"])
            .unwrap();
        format!("TMUX='{socket},0,0'")
    }

    /// Somebody's own `tmux new-session`: a session taw did not make.
    fn add(&self, name: &str, at: &Path) {
        self.0
            .run_ok(&["new-session", "-d", "-s", name, "-c", at.to_str().unwrap()])
            .unwrap();
    }

    /// Every session's name, in order.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .0
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_name)
            .collect();
        names.sort();
        names
    }

    /// The directory session `name` runs in, if there is such a session.
    fn path_of(&self, name: &str) -> Option<PathBuf> {
        self.0
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.session_name == name)
            .map(|s| PathBuf::from(s.session_path))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// One test's directories: a fake home, the projects and the hook scripts, all in a tempdir.
struct Sandbox {
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        // On macOS a temp directory is reached through a `/var` symlink, and tmux reports
        // the resolved path.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        Sandbox { root, _tmp: tmp }
    }

    /// `mkdir -p` under the sandbox; the canonical path.
    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::canonicalize(&path).unwrap()
    }

    /// An executable `/bin/sh` script at `rel`, its body after the shebang.
    fn script(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        crate::common::make_executable(&path);
        path
    }

    /// A hook that appends `<name> TAB <directory>` to the sandbox file `log` every time it
    /// is called, then runs `then`.
    fn recorder(&self, rel: &str, log: &str, then: &str) -> PathBuf {
        let log = self.root.join(log);
        self.script(
            rel,
            &format!(
                "printf '%s\\t%s\\n' \"$1\" \"$2\" >> '{}'\n{then}",
                log.display()
            ),
        )
    }

    /// What a [`Sandbox::recorder`] wrote to `log`: one `(name, directory)` per call.
    fn calls(&self, log: &str) -> Vec<(String, String)> {
        std::fs::read_to_string(self.root.join(log))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (name, dir) = line.split_once('\t').expect("a recorded call");
                (name.to_string(), dir.to_string())
            })
            .collect()
    }
}

/// The config a user's file would give: the two hooks as written, and zoxide off (it
/// defaults to on, and `open_or_create` then runs the real `zoxide add` on a temp path).
fn cfg(pre: Option<&str>, post: Option<&str>) -> Config {
    cfg_with_limit(pre, post, None)
}

/// [`cfg`] with `[sessions] hook_timeout_seconds` written as `seconds`, a TOML number such
/// as `1`, `0` or `0.5`. Without one, the default limit applies.
fn cfg_with_limit(pre: Option<&str>, post: Option<&str>, seconds: Option<&str>) -> Config {
    let mut text = String::from("[sessions]\nzoxide = false\n");
    for (key, value) in [("pre_create_hook", pre), ("post_create_hook", post)] {
        if let Some(value) = value {
            text.push_str(&format!(
                "{key} = {}\n",
                toml::Value::String(value.to_string())
            ));
        }
    }
    if let Some(seconds) = seconds {
        text.push_str(&format!("hook_timeout_seconds = {seconds}\n"));
    }
    toml::from_str(&text).expect("a config file")
}

fn open(s: &Server, cfg: &Config, home: &Path, dir: &Path) -> anyhow::Result<Outcome> {
    open_or_create(&s.0, cfg, dir.to_str().unwrap(), home)
}

fn created(name: &str) -> Outcome {
    Outcome {
        session_name: name.to_string(),
        created: true,
        collision: None,
    }
}

fn reused(name: &str) -> Outcome {
    Outcome {
        session_name: name.to_string(),
        created: false,
        collision: None,
    }
}

fn path_str(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

// ---- the hook value: a command line ----

#[test]
fn a_hook_value_may_carry_its_own_flags() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    // `--prefix work-` is part of the value; the name and the directory come after it.
    let hook = sb.script(
        "hooks/pre.sh",
        "[ \"$1\" = --prefix ] || exit 9\necho \"$2$3\"\n",
    );
    let cfg = cfg(Some(&format!("{} --prefix work-", hook.display())), None);

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, created("work-notes"));
    assert_eq!(s.path_of("work-notes"), Some(project));
}

#[test]
fn a_post_hook_value_may_carry_its_own_flags_too() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let log = sb.root.join("post.log");
    let hook = sb.script(
        "hooks/post.sh",
        &format!(
            "[ \"$1\" = --tag ] || exit 9\nprintf '%s %s %s\\n' \"$2\" \"$3\" \"$4\" > '{}'\n",
            log.display()
        ),
    );
    let cfg = cfg(None, Some(&format!("{} --tag layout", hook.display())));

    open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("layout notes {}\n", project.display())
    );
}

#[test]
fn a_hook_value_is_expanded_the_way_a_shell_expands_a_command() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let first = sb.dir("projects/notes");
    let second = sb.dir("projects/ideas");
    let s = Server::start(&home);
    let hook = sb.script("hooks/pre.sh", "echo \"$LABEL$1\"\n");

    // An assignment in front of the command, as the wizard's `eval` allows.
    let with_env = cfg(Some(&format!("LABEL=work- {}", hook.display())), None);
    assert_eq!(
        open(&s, &with_env, &home, &first).unwrap(),
        created("work-notes")
    );

    // A second command joined with `&&`: the two arguments go to the last one.
    let chained = cfg(Some(&format!("LABEL=chain- && {}", hook.display())), None);
    assert_eq!(
        open(&s, &chained, &home, &second).unwrap(),
        created("ideas"),
        "LABEL is set for the first command only, so the second sees none"
    );
}

#[test]
fn a_plain_path_still_runs_as_the_hook() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let hook = sb.script("hooks/pre.sh", "echo \"renamed-$1\"\n");
    let cfg = cfg(Some(hook.to_str().unwrap()), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("renamed-notes")
    );
}

#[test]
fn a_leading_tilde_in_the_hook_value_is_the_home_directory() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    sb.script("home/taw-hooks-under-test/pre.sh", "echo from-home\n");
    let cfg = cfg(Some("~/taw-hooks-under-test/pre.sh"), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("from-home")
    );
}

#[test]
fn a_tilde_with_flags_after_it_is_still_the_home_directory() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    sb.script("home/taw-hooks-under-test/pre.sh", "echo \"$2$3\"\n");
    let cfg = cfg(Some("~/taw-hooks-under-test/pre.sh --prefix work-"), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("work-notes")
    );
}

#[test]
fn a_home_directory_with_a_space_in_it_does_not_split_the_hook_path() {
    let sb = Sandbox::new();
    let home = sb.dir("my home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    sb.script("my home/hooks/pre.sh", "echo from-home\n");
    let cfg = cfg(Some("~/hooks/pre.sh"), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("from-home")
    );
}

#[test]
fn a_path_with_a_space_in_it_is_one_word_when_the_value_quotes_it() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let hook = sb.script("my hooks/pre.sh", "echo quoted\n");
    let cfg = cfg(Some(&format!("'{}'", hook.display())), None);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("quoted"));
}

#[test]
fn a_blank_hook_value_is_no_hook() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let cfg = cfg(Some(""), Some("   "));

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
}

// ---- the two arguments are never read by a shell ----

/// A directory named the way somebody hostile would name a checkout: quotes, a command
/// substitution, backticks, a variable, a glob, a `;` and a `&`. The wizard's own test does
/// the same with `touch`; here a substitution that ran would change the argument the hook
/// sees, and a `;` that ran would end the hook's shell early, so no file has to be written
/// to notice either.
const HOSTILE: &str = "it's \"a\" $(echo INJECTED) `echo INJECTED` $HOME * ; exit 7 & x";

#[test]
fn a_directory_name_reaches_the_hook_as_one_literal_argument() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir(&format!("projects/{HOSTILE}"));
    let s = Server::start(&home);
    let pre = sb.recorder("hooks/pre.sh", "pre.log", "");
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), Some(post.to_str().unwrap()));

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, created(HOSTILE));
    let expected = vec![(HOSTILE.to_string(), path_str(&project))];
    assert_eq!(sb.calls("pre.log"), expected);
    assert_eq!(sb.calls("post.log"), expected);
    assert_eq!(s.path_of(HOSTILE), Some(project));
}

#[test]
fn a_directory_name_reaches_a_hook_with_flags_as_one_literal_argument_too() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    // A relative `touch`, run from the sandbox, would leave a file there if the name were
    // ever read by a shell: the `cd` in the value keeps that inside the test's own tree.
    let name = "a $(touch pwned) b `touch pwned2` c";
    let project = sb.dir(&format!("projects/{name}"));
    let s = Server::start(&home);
    let hook = sb.recorder("hooks/pre.sh", "pre.log", "");
    let value = format!("cd '{}' && '{}' --flag", sb.root.display(), hook.display());
    let cfg = cfg(Some(&value), None);

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, created(name));
    assert!(
        !sb.root.join("pwned").exists() && !sb.root.join("pwned2").exists(),
        "a command in the directory's name ran"
    );
    // `--flag` is the hook's own first argument, so the name and the directory follow it.
    assert_eq!(
        sb.calls("pre.log"),
        [("--flag".to_string(), name.to_string())],
        "the recorder logs its first two arguments"
    );
}

// ---- what the pre hook may print ----

#[test]
fn two_lines_replace_the_name_and_the_directory() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let elsewhere = sb.dir("elsewhere/alt");
    let s = Server::start(&home);
    let pre = sb.recorder(
        "hooks/pre.sh",
        "pre.log",
        &format!("printf '%s\\n%s\\n' custom '{}'\n", elsewhere.display()),
    );
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), Some(post.to_str().unwrap()));

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, created("custom"));
    assert_eq!(
        s.path_of("custom"),
        Some(elsewhere.clone()),
        "the second line is where the session runs"
    );
    assert_eq!(
        sb.calls("pre.log"),
        [("notes".to_string(), path_str(&project))],
        "the pre hook was given what taw proposed"
    );
    assert_eq!(
        sb.calls("post.log"),
        [("custom".to_string(), path_str(&elsewhere))],
        "the post hook is given what the pre hook chose"
    );
}

#[test]
fn a_hook_may_change_only_the_directory() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let elsewhere = sb.dir("elsewhere/alt");
    let s = Server::start(&home);
    // The wizard's own test prints the name back and a new directory.
    let pre = sb.script(
        "hooks/pre.sh",
        &format!("echo \"$1\"\necho '{}'\n", elsewhere.display()),
    );
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    assert_eq!(s.path_of("notes"), Some(elsewhere));
}

#[test]
fn a_name_alone_keeps_the_directory() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "echo renamed\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("renamed"));
    assert_eq!(s.path_of("renamed"), Some(project));
}

#[test]
fn printing_nothing_keeps_both() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "exit 0\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    assert_eq!(s.path_of("notes"), Some(project));
}

#[test]
fn a_blank_line_keeps_what_it_stands_for() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let elsewhere = sb.dir("elsewhere/alt");
    let s = Server::start(&home);
    // The name line is empty, so only the directory changes.
    let pre = sb.script(
        "hooks/pre.sh",
        &format!("echo\necho '{}'\n", elsewhere.display()),
    );
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    assert_eq!(s.path_of("notes"), Some(elsewhere));
}

#[test]
fn a_name_the_hook_prints_gets_tmuxs_own_rule_and_nothing_more() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "echo 'My.App:v2 Work'\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("My_App_v2 Work"),
        "tmux's `.` and `:` become `_`; case and spaces stay"
    );
}

#[test]
fn a_directory_the_hook_prints_must_exist() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let missing = sb.root.join("no-such-dir");
    let s = Server::start(&home);
    let pre = sb.script(
        "hooks/pre.sh",
        &format!("echo custom\necho '{}'\n", missing.display()),
    );
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), Some(post.to_str().unwrap()));

    let err = open(&s, &cfg, &home, &project).unwrap_err().to_string();

    assert!(
        err.starts_with(&format!("no such directory: {}", missing.display()))
            && err.contains("pre-create hook"),
        "{err}"
    );
    assert_eq!(s.names(), ["main"], "nothing was created");
    assert!(sb.calls("post.log").is_empty(), "and no post hook ran");
}

#[test]
fn a_tilde_in_the_directory_the_hook_prints_is_the_home_directory() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let under_home = sb.dir("home/code/alt");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "echo custom\necho '~/code/alt'\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(s.path_of("custom"), Some(under_home));
}

// ---- when the pre hook runs: every open, before the running session is looked for ----

#[test]
fn the_pre_hook_runs_when_a_session_is_already_running_there() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    s.add("scratch", &project);
    let pre = sb.recorder("hooks/pre.sh", "pre.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, reused("scratch"));
    assert_eq!(
        sb.calls("pre.log"),
        [("notes".to_string(), path_str(&project))],
        "asked once, with the name a new session there would get"
    );
    assert_eq!(s.names(), ["main", "scratch"]);
}

#[test]
fn a_hook_that_hands_back_what_it_was_given_changes_nothing() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    s.add("scratch", &project);
    // The wizard's own tests keep a value by printing it back.
    let pre = sb.script("hooks/pre.sh", "echo \"$1\"\necho \"$2\"\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        reused("scratch"),
        "a session a hand-made name is already running in the directory is still found"
    );
    assert_eq!(s.names(), ["main", "scratch"]);
}

#[test]
fn the_pre_hook_can_name_another_session_for_a_directory_that_is_already_open() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    s.add("notes", &project);
    let pre = sb.script("hooks/pre.sh", "echo review\n");
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), Some(post.to_str().unwrap()));

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, created("review"));
    assert_eq!(s.names(), ["main", "notes", "review"]);
    assert_eq!(s.path_of("review"), Some(project.clone()));
    assert_eq!(
        sb.calls("post.log"),
        [("review".to_string(), path_str(&project))]
    );
}

#[test]
fn the_session_a_pre_hook_names_is_used_when_it_is_already_running() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let notes = sb.dir("projects/notes");
    let ideas = sb.dir("projects/ideas");
    let s = Server::start(&home);
    s.add("notes", &notes);
    s.add("review", &ideas);
    let pre = sb.script("hooks/pre.sh", "echo review\n");
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), Some(post.to_str().unwrap()));

    let outcome = open(&s, &cfg, &home, &notes).unwrap();

    assert_eq!(outcome, reused("review"));
    assert_eq!(s.names(), ["main", "notes", "review"], "nothing new");
    assert!(sb.calls("post.log").is_empty(), "a reuse is not a creation");
}

#[test]
fn a_name_that_a_hook_builds_from_the_one_it_was_given_finds_its_own_session_next_time() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    // The wizard's README example: prefix every session name.
    let pre = sb.script("hooks/pre.sh", "echo \"work-$1\"\necho \"$2\"\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("work-notes")
    );
    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        reused("work-notes"),
        "given the name taw proposes, not the one already running, so it does not grow"
    );
    assert_eq!(s.names(), ["main", "work-notes"]);
}

#[test]
fn a_hook_sees_the_session_list_as_it_is_when_it_answers() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    // A hook that makes the session itself, then answers with nothing: the open must use
    // the session that now exists and not try to make a second one under the same name.
    let make = sb.script(
        "hooks/pre.sh",
        &format!("{} tmux new-session -d -s \"$1\" -c \"$2\"\n", s.tmux_env()),
    );
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(make.to_str().unwrap()), Some(post.to_str().unwrap()));

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, reused("notes"));
    assert_eq!(s.names(), ["main", "notes"]);
    assert!(
        sb.calls("post.log").is_empty(),
        "taw did not create it, so no post hook"
    );
}

// ---- a non-zero pre hook stops the open ----

#[test]
fn a_pre_hook_that_exits_non_zero_creates_nothing_and_says_why() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "echo 'not today' >&2\nexit 3\n");
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(Some(pre.to_str().unwrap()), Some(post.to_str().unwrap()));

    let err = open(&s, &cfg, &home, &project).unwrap_err().to_string();

    assert!(
        err.contains("pre-create hook")
            && err.contains("exit status: 3")
            && err.contains("not today"),
        "{err}"
    );
    assert_eq!(s.names(), ["main"], "no session was created");
    assert!(sb.calls("post.log").is_empty(), "and no post hook ran");
}

#[test]
fn a_pre_hook_that_exits_non_zero_stops_the_open_of_a_running_directory_too() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    s.add("scratch", &project);
    let pre = sb.script("hooks/pre.sh", "exit 1\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    let err = open(&s, &cfg, &home, &project).unwrap_err().to_string();

    assert!(err.contains("pre-create hook"), "{err}");
    assert_eq!(s.names(), ["main", "scratch"]);
}

#[test]
fn a_pre_hook_that_is_not_there_stops_the_open() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let cfg = cfg(Some(&path_str(&sb.root.join("no-such-hook.sh"))), None);

    let err = open(&s, &cfg, &home, &project).unwrap_err().to_string();

    assert!(err.contains("pre-create hook"), "{err}");
    assert_eq!(s.names(), ["main"]);
}

// ---- the post hook ----

#[test]
fn the_post_hook_runs_once_for_a_new_session_and_never_for_a_reuse() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg(None, Some(post.to_str().unwrap()));

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), reused("notes"));
    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), reused("notes"));

    assert_eq!(
        sb.calls("post.log"),
        [("notes".to_string(), path_str(&project))],
        "the two reuses did not run it"
    );
}

#[test]
fn the_post_hook_runs_once_the_session_exists() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let log = sb.root.join("post.log");
    // Asks the server, from inside the hook, whether the session it was given is there yet.
    let post = sb.script(
        "hooks/post.sh",
        &format!(
            "{} tmux has-session -t \"=$1\" && echo there > '{}'\n",
            s.tmux_env(),
            log.display()
        ),
    );
    let cfg = cfg(None, Some(post.to_str().unwrap()));

    open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(std::fs::read_to_string(&log).unwrap(), "there\n");
}

#[test]
fn a_post_hook_that_fails_or_prints_changes_nothing() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let elsewhere = sb.dir("elsewhere/alt");
    let s = Server::start(&home);
    // Its output is not an answer, and a failure is logged, not raised.
    let post = sb.script(
        "hooks/post.sh",
        &format!(
            "echo renamed\necho '{}'\necho 'noise' >&2\nexit 1\n",
            elsewhere.display()
        ),
    );
    let cfg = cfg(None, Some(post.to_str().unwrap()));

    let outcome = open(&s, &cfg, &home, &project).unwrap();

    assert_eq!(outcome, created("notes"));
    assert_eq!(s.names(), ["main", "notes"]);
    assert_eq!(s.path_of("notes"), Some(project));
}

#[test]
fn a_post_hook_that_is_not_there_does_not_undo_the_session() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let cfg = cfg(None, Some(&path_str(&sb.root.join("no-such-hook.sh"))));

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    assert_eq!(s.names(), ["main", "notes"]);
}

/// The way a hook starts something in the background without saying where it writes: the
/// child holds the hook's stdout and stderr open until it ends. tmux-session-wizard sends a
/// post hook's output to /dev/null, so it never waits for such a child; the open here must
/// not either, or the popup would sit there, and never switch, until the child was gone.
#[test]
fn a_post_hook_that_leaves_a_process_running_does_not_hold_the_open_up() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pid_file = sb.root.join("leftover.pid");
    let post = sb.script(
        "hooks/post.sh",
        &format!("sleep 12 &\necho $! > '{}'\n", pid_file.display()),
    );
    let cfg = cfg(None, Some(post.to_str().unwrap()));

    let started = std::time::Instant::now();
    let outcome = open(&s, &cfg, &home, &project);
    let took = started.elapsed();

    // Tidy up the child before anything can fail.
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
    assert_eq!(outcome.unwrap(), created("notes"));
    assert!(
        took < std::time::Duration::from_secs(8),
        "the open waited {took:?} for a process the hook left running"
    );
}

// ---- how long a hook may take: [sessions] hook_timeout_seconds ----

/// How much longer than a limit a test lets an open take to come back: far more than
/// stopping a hook needs, and well short of the 30 seconds the slow hooks below sleep, so
/// a limit that did not work cannot pass for one that did.
const MARGIN: Duration = Duration::from_secs(8);

/// The limit of the tests that look at what a hook started. The first run of a
/// script that was just written can take a second to begin on a machine that scans new
/// executables, and the limit has to leave what the hook starts the time to be started.
const LIMIT: Duration = Duration::from_secs(3);

/// Whether the process `pid` is still there (`kill -0`).
fn is_running(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the process `pid` is gone within a few seconds: it dies with the hook that
/// started it, and whatever reaps it may take a moment.
fn is_gone_soon(pid: &str) -> bool {
    for _ in 0..40 {
        if !is_running(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Ends `pid` if it is still there, so a test that failed does not leave a `sleep 30`.
fn stop(pid: &str) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", pid])
        .stderr(std::process::Stdio::null())
        .status();
}

/// The pid a hook wrote to `file` with `echo $! > file`.
fn pid_in(file: &Path) -> String {
    std::fs::read_to_string(file)
        .expect("the hook wrote a pid")
        .trim()
        .to_string()
}

#[test]
fn a_pre_hook_that_runs_past_the_limit_is_stopped_and_refuses_the_open() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pid_file = sb.root.join("slow.pid");
    let pre = sb.script(
        "hooks/pre.sh",
        &format!("sleep 30 &\necho $! > '{}'\nwait\n", pid_file.display()),
    );
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg_with_limit(
        Some(pre.to_str().unwrap()),
        Some(post.to_str().unwrap()),
        Some("3"),
    );

    let started = Instant::now();
    let result = open(&s, &cfg, &home, &project);
    let took = started.elapsed();

    let slow = pid_in(&pid_file);
    let stopped = is_gone_soon(&slow);
    stop(&slow);
    let err = result.unwrap_err().to_string();
    assert_eq!(err, "pre-create hook took longer than 3 s");
    assert!(
        took >= LIMIT && took < LIMIT + MARGIN,
        "the open took {took:?} with a limit of 3 s"
    );
    assert!(stopped, "the hook's own child was left running");
    assert_eq!(s.names(), ["main"], "nothing was created");
    assert!(sb.calls("post.log").is_empty(), "and no post hook ran");
}

#[test]
fn a_pre_hook_that_runs_past_the_limit_refuses_the_open_of_a_running_directory_too() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    s.add("scratch", &project);
    let pre = sb.script("hooks/pre.sh", "sleep 30\n");
    let cfg = cfg_with_limit(Some(pre.to_str().unwrap()), None, Some("1"));

    let started = Instant::now();
    let err = open(&s, &cfg, &home, &project).unwrap_err().to_string();

    assert_eq!(err, "pre-create hook took longer than 1 s");
    assert!(started.elapsed() < Duration::from_secs(1) + MARGIN);
    assert_eq!(s.names(), ["main", "scratch"]);
}

#[test]
fn a_post_hook_that_runs_past_the_limit_is_stopped_and_the_session_stays() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pid_file = sb.root.join("slow.pid");
    let post = sb.script(
        "hooks/post.sh",
        &format!("sleep 30 &\necho $! > '{}'\nwait\n", pid_file.display()),
    );
    let cfg = cfg_with_limit(None, Some(post.to_str().unwrap()), Some("3"));

    let started = Instant::now();
    let result = open(&s, &cfg, &home, &project);
    let took = started.elapsed();

    let slow = pid_in(&pid_file);
    let stopped = is_gone_soon(&slow);
    stop(&slow);
    assert_eq!(
        result.unwrap(),
        created("notes"),
        "a post hook's failure is logged, and the session it ran after stays"
    );
    assert!(
        took >= LIMIT && took < LIMIT + MARGIN,
        "the open took {took:?} with a limit of 3 s"
    );
    assert!(stopped, "the hook's own child was left running");
    assert_eq!(s.names(), ["main", "notes"]);
}

/// The hook exits at once but leaves a process that holds its output open: taw reads that
/// output to the end, so it waits for the process, and the limit is what ends the wait.
#[test]
fn a_pre_hook_that_leaves_a_process_holding_its_output_is_waited_for_until_the_limit() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pid_file = sb.root.join("left.pid");
    let pre = sb.script(
        "hooks/pre.sh",
        &format!("sleep 30 &\necho $! > '{}'\n", pid_file.display()),
    );
    let cfg = cfg_with_limit(Some(pre.to_str().unwrap()), None, Some("3"));

    let started = Instant::now();
    let result = open(&s, &cfg, &home, &project);
    let took = started.elapsed();

    stop(&pid_in(&pid_file));
    assert_eq!(
        result.unwrap_err().to_string(),
        "pre-create hook took longer than 3 s"
    );
    assert!(
        took >= LIMIT && took < LIMIT + MARGIN,
        "the open took {took:?} with a limit of 3 s"
    );
    assert_eq!(s.names(), ["main"]);
}

/// The same process with its output sent away is nothing to wait for: the hook answered and
/// left, and the open goes on.
#[test]
fn a_pre_hook_that_leaves_a_process_with_its_output_redirected_is_not_waited_for() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pid_file = sb.root.join("left.pid");
    let pre = sb.script(
        "hooks/pre.sh",
        &format!(
            "sleep 30 >/dev/null 2>&1 &\necho $! > '{}'\n",
            pid_file.display()
        ),
    );
    let cfg = cfg_with_limit(Some(pre.to_str().unwrap()), None, Some("5"));

    let started = Instant::now();
    let result = open(&s, &cfg, &home, &project);
    let took = started.elapsed();

    stop(&pid_in(&pid_file));
    assert_eq!(result.unwrap(), created("notes"));
    assert!(
        took < Duration::from_secs(5),
        "the open waited {took:?} for a process that holds none of the hook's output"
    );
}

/// `0` is how the limit is switched off, not a limit of no time at all.
#[test]
fn a_limit_of_zero_means_a_hook_may_take_as_long_as_it_likes() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "sleep 1\necho slow\n");
    let post = sb.script("hooks/post.sh", "sleep 1\n");
    let cfg = cfg_with_limit(
        Some(pre.to_str().unwrap()),
        Some(post.to_str().unwrap()),
        Some("0"),
    );

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("slow"));
    assert_eq!(s.names(), ["main", "slow"]);
}

#[test]
fn a_hook_that_finishes_inside_the_limit_is_left_alone() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "echo quick\n");
    let post = sb.recorder("hooks/post.sh", "post.log", "");
    let cfg = cfg_with_limit(
        Some(pre.to_str().unwrap()),
        Some(post.to_str().unwrap()),
        Some("5"),
    );

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("quick"));
    assert_eq!(
        sb.calls("post.log"),
        [("quick".to_string(), path_str(&project))]
    );
}

/// With no `hook_timeout_seconds` in the file the default limit is in force, and a hook of
/// ordinary length is nowhere near it.
#[test]
fn a_config_without_a_limit_lets_an_ordinary_hook_through() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let pre = sb.script("hooks/pre.sh", "sleep 1\necho ordinary\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("ordinary")
    );
}

// ---- what a hook is given to read ----

/// Set in the copy of [`a_hook_is_given_no_standard_input`] that is run with a standard input
/// that stays open.
const LIVE_STDIN: &str = "TAW_TEST_HOOKS_LIVE_STDIN";

/// A hook has no standard input to read: it is `/dev/null`, so a `cat` in a hook ends at once
/// instead of waiting for a terminal, or for whatever `taw` was started with, to send a line.
///
/// That only shows when the tests are run with a standard input that could be waited on, and a
/// runner that gives them `/dev/null` would let a hook that inherited it pass. So the test runs
/// itself again with a standard input that is an open pipe nothing is ever written to, and looks
/// at how that copy ended. The limit is a safety net: a hook stuck on that pipe fails the copy
/// after five seconds instead of holding it up.
#[test]
fn a_hook_is_given_no_standard_input() {
    const NAME: &str = "a_hook_is_given_no_standard_input";
    if std::env::var_os(LIVE_STDIN).is_none() {
        let mut copy = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1"])
            .env(LIVE_STDIN, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The write end of the pipe stays open until the copy is done, or `wait` would close it.
        let _open = copy.stdin.take();
        let started = Instant::now();
        let status = loop {
            if let Some(status) = copy.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > Duration::from_secs(60) {
                let _ = copy.kill();
                panic!("the copy of this test with an open standard input is still running");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut said = String::new();
        if let Some(mut out) = copy.stdout.take() {
            let _ = std::io::Read::read_to_string(&mut out, &mut said);
        }
        if let Some(mut err) = copy.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut err, &mut said);
        }
        assert!(
            status.success(),
            "a hook waited on the standard input it was started with:\n{said}"
        );
        return;
    }

    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let reached = sb.root.join("post.reached");
    let pre = sb.script("hooks/pre.sh", "cat >/dev/null\necho read-nothing\n");
    let post = sb.script(
        "hooks/post.sh",
        &format!("cat >/dev/null\n: > '{}'\n", reached.display()),
    );
    let cfg = cfg_with_limit(
        Some(pre.to_str().unwrap()),
        Some(post.to_str().unwrap()),
        Some("5"),
    );

    assert_eq!(
        open(&s, &cfg, &home, &project).unwrap(),
        created("read-nothing")
    );
    assert!(reached.exists(), "the post hook did not get past its `cat`");
}

// ---- the collision note ----

/// The outcome of opening `~/oss/api` (`~` being `home`) while `api` is `~/work/api`'s session:
/// the second `api` is told apart by a longer name, and the note says why.
fn collision_outcome(name: &str, note: bool) -> Outcome {
    Outcome {
        session_name: name.to_string(),
        created: true,
        collision: note.then(|| "\"api\" is taken by ~/work/api".to_string()),
    }
}

#[test]
fn a_hook_that_leaves_the_name_alone_leaves_the_collision_note_in_place() {
    let sb = Sandbox::new();
    let work = sb.dir("work/api");
    let oss = sb.dir("oss/api");
    let s = Server::start(&sb.dir("home"));
    s.add("api", &work);
    let pre = sb.script("hooks/pre.sh", "exit 0\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    let outcome = open(&s, &cfg, &sb.root, &oss).unwrap();

    assert_eq!(outcome, collision_outcome("oss/api", true));
    assert_eq!(s.path_of("oss/api"), Some(oss));
}

#[test]
fn a_hook_that_prints_the_name_back_leaves_the_collision_note_in_place_too() {
    let sb = Sandbox::new();
    let work = sb.dir("work/api");
    let oss = sb.dir("oss/api");
    let s = Server::start(&sb.dir("home"));
    s.add("api", &work);
    // The wizard's own tests keep a value by printing it back.
    let pre = sb.script("hooks/pre.sh", "echo \"$1\"\necho \"$2\"\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    let outcome = open(&s, &cfg, &sb.root, &oss).unwrap();

    assert_eq!(outcome, collision_outcome("oss/api", true));
}

/// The note explains the name taw proposed. A name the hook chose is not that name, so it
/// would explain something that did not happen.
#[test]
fn a_hook_that_chooses_another_name_drops_the_collision_note() {
    let sb = Sandbox::new();
    let work = sb.dir("work/api");
    let oss = sb.dir("oss/api");
    let s = Server::start(&sb.dir("home"));
    s.add("api", &work);
    let pre = sb.script("hooks/pre.sh", "echo review\n");
    let cfg = cfg(Some(pre.to_str().unwrap()), None);

    let outcome = open(&s, &cfg, &sb.root, &oss).unwrap();

    assert_eq!(outcome, collision_outcome("review", false));
    assert_eq!(s.path_of("review"), Some(oss));
}

// ---- the wizard's own example hooks, unchanged ----

/// The names of `session`'s windows, in order.
fn windows(s: &Server, session: &str) -> Vec<String> {
    s.0.run(&[
        "list-windows",
        "-t",
        &format!("={session}"),
        "-F",
        "#{window_name}",
    ])
    .unwrap()
    .lines()
    .map(String::from)
    .collect()
}

/// `examples/project-layout-hook.sh` and `examples/resolve-session-conflict-hook.sh` from
/// tmux-session-wizard, copied with a provenance line. They call a plain `tmux`, so each
/// value starts with `$TMUX` for the throwaway server.
fn wizard_example(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/session-wizard-hooks")
        .join(name)
        .to_str()
        .unwrap()
        .to_string()
}

#[test]
fn the_wizards_layout_example_builds_windows_for_a_new_session_and_only_then() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    sb.dir("projects/notes/.git");
    let s = Server::start(&home);
    let post = format!(
        "{} {}",
        s.tmux_env(),
        wizard_example("project-layout-hook.sh")
    );
    let cfg = cfg(None, Some(&post));

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    assert_eq!(windows(&s, "notes"), ["editor", "shell", "git"]);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), reused("notes"));
    assert_eq!(
        windows(&s, "notes"),
        ["editor", "shell", "git"],
        "a reuse builds nothing more"
    );
}

#[test]
fn the_wizards_conflict_example_leaves_a_session_for_the_same_directory_alone() {
    let sb = Sandbox::new();
    let home = sb.dir("home");
    let project = sb.dir("projects/notes");
    let s = Server::start(&home);
    let recorder = sb.recorder("hooks/asked.sh", "asked.log", "");
    // The recorder is asked first, with the two arguments spelled out, so the log shows
    // every time the example was. `$TMPDIR` is where the example keeps its remembered
    // names: the sandbox, not the developer's own.
    let pre = format!(
        "'{}' \"$@\" && TMPDIR='{}' {} {}",
        recorder.display(),
        sb.root.display(),
        s.tmux_env(),
        wizard_example("resolve-session-conflict-hook.sh")
    );
    let cfg = cfg(Some(&pre), None);

    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), created("notes"));
    // The second open is the case the example is written for: the name is taken, by the
    // same directory, so it prints nothing and the running session is used.
    assert_eq!(open(&s, &cfg, &home, &project).unwrap(), reused("notes"));
    assert_eq!(s.names(), ["main", "notes"]);
    let asked = ("notes".to_string(), path_str(&project));
    assert_eq!(
        sb.calls("asked.log"),
        [asked.clone(), asked],
        "asked on the second open as well as the first"
    );
}

// ------------------------------------------------- a tmux with no server running yet

/// A `tmux` that starts its server the way every other test server here is started: no
/// config file, `/bin/sh` in its panes, and the home and the history file that `home` makes
/// (`common::shell_env`). An open that finds no server is the one that starts it, with a
/// `new-session` of its own, so all of that has to be on the binary.
fn quiet_tmux(dir: &Path, socket: &str, home: &ShellHome) -> Tmux {
    let bin = dir.join("tmux-quiet");
    let home = home.path().display();
    std::fs::write(
        &bin,
        format!("#!/bin/sh\nHISTFILE=/dev/null HOME='{home}' ZDOTDIR='{home}' SHELL=/bin/sh exec tmux -f /dev/null \"$@\"\n"),
    )
    .unwrap();
    crate::common::make_executable(&bin);
    Tmux {
        bin,
        socket: Some(taw::tmux::Socket::Name(socket.to_string())),
    }
}

/// `taw open` from a plain terminal, with tmux not running at all: there are no sessions to
/// look through, so the open goes on to `new-session`, which starts the server.
#[test]
fn an_open_with_no_server_running_starts_one() {
    let sandbox = Sandbox::new();
    let project = sandbox.dir("work/api");
    let socket = format!("taw-test-hooks-cold-{}", std::process::id());
    let home = ShellHome::new();
    let server = Server(quiet_tmux(&sandbox.root, &socket, &home), home);
    assert!(
        taw::tmux::no_server(&server.0.list_sessions().unwrap_err()),
        "nothing is listening on the socket before the open"
    );

    let outcome = open(&server, &cfg(None, None), &sandbox.root, &project).unwrap();
    // The open started this server, so the home has to be told whose socket file to take away.
    server.1.adopt(&server.0);

    assert_eq!(outcome, created("api"));
    assert_eq!(server.names(), vec!["api"]);
    assert_eq!(server.path_of("api"), Some(project));
}

/// The pre-create hook runs before the running sessions are looked at a second time, and
/// with no server that second look finds none either.
#[test]
fn a_pre_create_hook_runs_when_there_is_no_server_yet() {
    let sandbox = Sandbox::new();
    let project = sandbox.dir("work/api");
    let hook = sandbox.recorder("pre.sh", "pre.log", "");
    let socket = format!("taw-test-hooks-cold-hook-{}", std::process::id());
    let home = ShellHome::new();
    let server = Server(quiet_tmux(&sandbox.root, &socket, &home), home);

    let outcome = open(
        &server,
        &cfg(Some(hook.to_str().unwrap()), None),
        &sandbox.root,
        &project,
    )
    .unwrap();
    server.1.adopt(&server.0);

    assert_eq!(outcome, created("api"));
    assert_eq!(
        sandbox.calls("pre.log"),
        vec![("api".to_string(), project.display().to_string())]
    );
    assert_eq!(server.names(), vec!["api"]);
}
