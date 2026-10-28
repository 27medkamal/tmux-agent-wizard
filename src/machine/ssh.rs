//! The ssh runner: every ssh taw runs is built here, so that the options, the control socket and the
//! quoting of what runs on the other machine are in one place, and a test has one seam to stand in
//! for ssh (`TAW_SSH_BIN`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::Paths;

use super::Machine;

/// The program that is run as ssh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ssh {
    pub bin: PathBuf,
}

impl Ssh {
    /// `$TAW_SSH_BIN` when it is set and not empty, else `ssh`, found on the `PATH` when it runs.
    /// The variable is the seam a test puts its stand-in behind: no test of taw runs a real ssh.
    pub fn from_env() -> Ssh {
        Ssh::from_var(std::env::var_os("TAW_SSH_BIN"))
    }

    fn from_var(value: Option<std::ffi::OsString>) -> Ssh {
        match value.filter(|bin| !bin.is_empty()) {
            Some(bin) => Ssh {
                bin: PathBuf::from(bin),
            },
            None => Ssh {
                bin: PathBuf::from("ssh"),
            },
        }
    }
}

/// What the ssh is for, which decides how it holds the terminal and the control socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A long-lived feed: no terminal, and standard input left open for what is written to it.
    Feed,
    /// One command, whose reply is read: no terminal, standard input closed.
    Command,
    /// Going in: a terminal, which the remote program draws on.
    Attach,
    /// One command that keeps nothing: it opens its own connection, uses no control socket and
    /// leaves none (`taw machine check`).
    Fresh,
}

/// The most bytes a control socket's path may have. macOS allows 104 for a socket's path, and ssh binds
/// the path with a 17-byte temporary suffix first.
const CONTROL_PATH_MAX: usize = 86;

/// The ssh command that runs `taw <args>` on `m`, as `mode` says. Its standard streams and its
/// running are the caller's; it has `SSH_ASKPASS_REQUIRE=never` so that nothing is ever asked.
///
/// The options are the ones the design lists, in its order: given on the command line they win over
/// the user's ssh config for those lines only, and everything else (user, port, key, `ProxyJump`)
/// still comes from it. A connection that is opened is kept for a minute (`ControlPersist`), so the
/// next command rides it.
pub fn command(ssh: &Ssh, paths: &Paths, m: &Machine, mode: Mode, args: &[&str]) -> Command {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    command_in(
        ssh,
        paths,
        m,
        mode,
        args,
        &Places {
            tmpdir: shared_tmpdir_left_out(std::env::var_os("TMPDIR")),
            tmp: PathBuf::from("/tmp"),
            uid,
        },
    )
}

/// What the control socket's place depends on besides the state folder: where `$TMPDIR` points, the
/// shared folder of last resort (`/tmp`), and who this is. A test gives its own.
struct Places {
    tmpdir: Option<std::ffi::OsString>,
    tmp: PathBuf,
    uid: u32,
}

/// `tmpdir` unless it is a folder anyone can write in (mode `o+w`, as `/tmp` itself is, sticky or not):
/// a socket with a guessable name put straight there could be taken first by another user. Then there
/// is no `$TMPDIR` for the chain, which goes on to the checked `<tmp>/taw-<uid>/` folder. A folder
/// that cannot be looked at is left as it is: there is nothing to say it is shared.
fn shared_tmpdir_left_out(tmpdir: Option<std::ffi::OsString>) -> Option<std::ffi::OsString> {
    use std::os::unix::fs::PermissionsExt;
    tmpdir.filter(|dir| match std::fs::metadata(dir) {
        Ok(meta) => meta.permissions().mode() & 0o002 == 0,
        Err(_) => true,
    })
}

fn command_in(
    ssh: &Ssh,
    paths: &Paths,
    m: &Machine,
    mode: Mode,
    args: &[&str],
    places: &Places,
) -> Command {
    // The config refuses a destination that starts with a dash; this is the second lock, since ssh would
    // read such a destination as an option.
    assert!(
        !m.ssh.starts_with('-'),
        "an ssh destination cannot start with -: {:?}",
        m.ssh
    );
    let mut cmd = Command::new(&ssh.bin);
    match mode {
        Mode::Feed => cmd.arg("-T"),
        Mode::Command | Mode::Fresh => cmd.args(["-T", "-n"]),
        Mode::Attach => cmd.arg("-t"),
    };
    cmd.arg("-C");
    for option in [
        "BatchMode=yes",
        "NumberOfPasswordPrompts=0",
        "StrictHostKeyChecking=yes",
        "ConnectTimeout=10",
        "ConnectionAttempts=1",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=4",
    ] {
        cmd.args(["-o", option]);
    }
    let control = match mode {
        Mode::Fresh => None,
        _ => prepared_control_path(paths, &m.name, places),
    };
    match control {
        None => {
            cmd.args(["-o", "ControlPath=none"]);
        }
        Some(path) => {
            cmd.args(["-o", "ControlMaster=auto"]);
            cmd.arg("-o");
            let mut control = std::ffi::OsString::from("ControlPath=");
            control.push(path);
            cmd.arg(control);
            cmd.args(["-o", "ControlPersist=60"]);
        }
    }
    cmd.arg(&m.ssh).arg(remote_line(m, args));
    cmd.env("SSH_ASKPASS_REQUIRE", "never");
    cmd
}

/// Makes the folder ssh's control sockets live in, readable by the user alone, and keeps it so.
fn make_control_dir(dir: &Path) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let made = match dir.parent() {
        Some(parent) => std::fs::create_dir_all(parent),
        None => Ok(()),
    }
    .and_then(|()| {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .or_else(already_there)
    })
    .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)));
    if let Err(err) = made {
        // ssh then says it cannot make its socket and goes on without one: the command still runs.
        log::warn!("could not make {} private: {err}", dir.display());
    }
}

fn already_there(err: std::io::Error) -> std::io::Result<()> {
    if err.kind() == std::io::ErrorKind::AlreadyExists {
        Ok(())
    } else {
        Err(err)
    }
}

/// How a machine without a `taw` of its own is looked for: `taw` on the `PATH` the shell there gives,
/// then the plugin's own folder: `bin/taw` in the state folder (under `XDG_STATE_HOME` when that is an
/// absolute path, as `scripts/helpers.sh` decides, then `~/.local/state/tmux-agent-wizard`), which is
/// only a link the plugin keeps there while `@agent-wizard-path` is on, then the binary `prefix + I`
/// installs in TPM's plugin folder (`~/.tmux/plugins/tmux-agent-wizard`, then
/// `tmux/plugins/tmux-agent-wizard` under `XDG_CONFIG_HOME` when that is an absolute path, then under
/// `~/.config`), then the places an install puts it. The plugin's folders come before the others
/// because the `taw` there is the one that machine's tmux runs (a non-interactive ssh rarely has it on
/// its `PATH`), while an older copy in `~/.local/bin` may be left over. A word with a `/` in it is a
/// file to run, any other a command to look up. Nothing is cached: each connection looks again.
const FINDER: &str = "case ${XDG_STATE_HOME:-} in /*) s=$XDG_STATE_HOME ;; *) s=$HOME/.local/state ;; esac; \
case ${XDG_CONFIG_HOME:-} in /*) c=$XDG_CONFIG_HOME ;; *) c=$HOME/.config ;; esac; \
for t in taw \"$s/tmux-agent-wizard/bin/taw\" \"$HOME/.local/state/tmux-agent-wizard/bin/taw\" \
\"$HOME/.tmux/plugins/tmux-agent-wizard/bin/taw\" \"$c/tmux/plugins/tmux-agent-wizard/bin/taw\" \
\"$HOME/.config/tmux/plugins/tmux-agent-wizard/bin/taw\" \
\"$HOME/.local/bin/taw\" \"$HOME/.cargo/bin/taw\" /opt/homebrew/bin/taw /usr/local/bin/taw; \
do case $t in */*) [ -x \"$t\" ] && exec \"$t\" \"$@\" ;; \
*) command -v \"$t\" >/dev/null 2>&1 && exec \"$t\" \"$@\" ;; esac; done; \
echo \"sh: taw: command not found\" >&2; exit 127";

/// What runs the file `path` (already quoted) with the arguments, or says there is no taw and exits 127,
/// whichever shell `sh` is: `exec` of a missing file exits 127 in dash and 126 in bash.
fn run_if_there(path: &str) -> String {
    format!(
        "[ -x {path} ] && exec {path} \"$@\"; echo \"sh: taw: command not found\" >&2; exit 127"
    )
}

/// A word for a POSIX shell, in single quotes whatever it holds.
fn single_quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// What is given to ssh to run on the other machine: `sh -c '<finder>' taw <args>`, each argument
/// quoted so that the shell there reads back exactly the words given, whatever they hold (a space,
/// `$(...)`, a quote, a newline). The program that runs is the machine's `taw` when it has one
/// (`~/` at its start is that machine's home), else the one the finder looks for.
///
/// The line is for a login shell that reads POSIX quoting: fish and csh read it the same except for an
/// argument holding a backslash.
pub fn remote_line(m: &Machine, args: &[&str]) -> String {
    let finder = match &m.taw {
        Some(path) => match path.strip_prefix("~/") {
            Some(rest) => run_if_there(&format!("\"$HOME\"/{}", single_quoted(rest))),
            None => run_if_there(&single_quoted(path)),
        },
        None => FINDER.to_string(),
    };
    // Always single quotes: a word that `shell_words::quote` leaves bare (`{a,b}`) is still
    // expanded by the shell there.
    let mut line = format!("sh -c {} taw", single_quoted(&finder));
    for arg in args {
        line.push(' ');
        line.push_str(&single_quoted(arg));
    }
    line
}

/// Where a machine's control socket goes, and so which folder holds it.
#[derive(Debug, PartialEq, Eq)]
enum Control {
    /// `<state>/m/<name>`.
    Own(PathBuf),
    /// `$TMPDIR/taw-<uid>-<8 hex>`: `$TMPDIR` is there already.
    Tmpdir(PathBuf),
    /// `<tmp>/taw-<uid>/<8 hex>`, in a folder of ours that has to be made and checked.
    Last(PathBuf),
}

#[cfg(test)]
impl Control {
    fn into_path(self) -> PathBuf {
        match self {
            Control::Own(path) | Control::Tmpdir(path) | Control::Last(path) => path,
        }
    }
}

/// The socket a command uses, with the folder that holds it made: `None` when that folder cannot be
/// trusted, and then ssh runs with `ControlPath=none` (nothing is shared, nothing persists) and the
/// reason is logged. Only the folder of the chosen socket is made.
fn prepared_control_path(paths: &Paths, name: &str, places: &Places) -> Option<PathBuf> {
    match checked_control_path(paths, name, places) {
        Ok(path) => Some(path),
        Err(warning) => {
            log::warn!("{warning}");
            None
        }
    }
}

/// What `prepared_control_path` does, with the warning it logs as the error: the words a person reads
/// when a folder of ours cannot be trusted and nothing is shared.
fn checked_control_path(paths: &Paths, name: &str, places: &Places) -> Result<PathBuf, String> {
    let choice = control_choice(
        paths,
        name,
        places.tmpdir.as_deref(),
        &places.tmp,
        places.uid,
    );
    match choice {
        Control::Own(path) => {
            make_control_dir(&paths.control_dir());
            Ok(path)
        }
        Control::Tmpdir(path) => Ok(path),
        Control::Last(path) => match private_folder(&places.tmp, places.uid) {
            Ok(_) => Ok(path),
            Err(why) => Err(format!("no shared ssh connection for {name}: {why}")),
        },
    }
}

/// Whether ssh can be given `path` as a control path as it is: absolute, short enough for a socket,
/// and only bytes that neither ssh's option reader nor a shell reads as anything but themselves.
fn usable_control_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    path.is_absolute()
        && bytes.len() <= CONTROL_PATH_MAX
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"/._+,:@-".contains(b))
}

/// The control socket of a machine's connection: `<state>/m/<name>`, or, where that would be too
/// long for a socket or ssh would misread it (a space splits an option line, `%` starts a token, a
/// quote or a backslash is read as quoting: only `A-Za-z0-9/._+,:@-` is kept), `$TMPDIR/taw-<uid>-<8 hex>`,
/// and without a usable `$TMPDIR` `/tmp/taw-<uid>/<8 hex>`. The digits are the start of the SHA-256 of
/// the state folder, a NUL and the name.
fn control_choice(
    paths: &Paths,
    name: &str,
    tmpdir: Option<&OsStr>,
    tmp: &Path,
    uid: u32,
) -> Control {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let own = paths.control_dir().join(name);
    if usable_control_path(&own) {
        return Control::Own(own);
    }
    let mut hasher = Sha256::new();
    hasher.update(paths.state_dir.as_os_str().as_bytes());
    hasher.update([0u8]);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    tmpdir
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(format!("taw-{uid}-{hex}")))
        .filter(|path| usable_control_path(path))
        .map(Control::Tmpdir)
        .unwrap_or_else(|| Control::Last(tmp.join(format!("taw-{uid}")).join(hex)))
}

/// `<tmp>/taw-<uid>`, made with mode 0700 when it is not there, and, when it is, only if it is a real
/// folder (not a link or a file), owned by `uid`, with mode exactly 0700: a folder in a shared place
/// that another user made first could hold a socket that is not ssh's.
fn private_folder(tmp: &Path, uid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let dir = tmp.join(format!("taw-{uid}"));
    let said = |why: &dyn std::fmt::Display| format!("{}: {why}", dir.display());
    let made = match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(err) => return Err(said(&err)),
    };
    if made {
        // A umask can only have taken bits away; this puts them back.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|err| said(&err))?;
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|err| said(&err))?;
    if !meta.file_type().is_dir() {
        return Err(said(&"is not a folder (a link or a file is there)"));
    }
    if meta.uid() != uid {
        return Err(said(&"is another user's folder"));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode != 0o700 {
        return Err(said(&format!("has mode {mode:o}, not 700")));
    }
    Ok(dir)
}

/// The last line ssh said on standard error that says something, as plain text and not cut: what it
/// says when it cannot connect (`ssh: connect to host studio.local port 22: Operation timed out`) is
/// the last of it. What is not text (an escape sequence, a control character, a bad byte) is taken out
/// or replaced, since the line goes into a message a person reads. It is not cut here: the words that
/// say why ssh failed (`Permission denied`) come at the end of a line that starts with the destination,
/// and a cut would hide them from [`super::words::classify`]. [`shown`] cuts it for the screen.
pub fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(|line| super::clean::plain_uncut(line).trim().to_string())
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
}

/// What of `line` (from [`last_line`]) is shown to a person: at most [`super::clean::MAX_TEXT`]
/// characters.
pub fn shown(line: &str) -> String {
    super::clean::cut(line)
}

/// `line` as plain text: the removal of escape sequences and control characters is
/// [`super::clean::plain`]'s, the one cleaner for text from another machine.
pub(crate) fn plain(line: &str) -> String {
    super::clean::plain(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(taw: Option<&str>) -> Machine {
        Machine {
            name: "studio".to_string(),
            ssh: "studio.local".to_string(),
            taw: taw.map(str::to_string),
        }
    }

    fn paths_in(state: &str) -> (Paths, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(dir.path().join("config"), PathBuf::from(state));
        (paths, dir)
    }

    /// The socket a machine gets with no `$TMPDIR` and the real `/tmp` as the last resort.
    fn control_path(paths: &Paths, name: &str) -> PathBuf {
        control_choice(paths, name, None, Path::new("/tmp"), 501).into_path()
    }

    fn argv(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn env_of(command: &Command, name: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    /// The options every mode shares, in order, up to the connection ones.
    fn common() -> Vec<&'static str> {
        vec![
            "-C",
            "-o",
            "BatchMode=yes",
            "-o",
            "NumberOfPasswordPrompts=0",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=4",
        ]
    }

    fn expected(bin: &str, terminal: &[&str], control: &[String], line: &str) -> Vec<String> {
        let mut want: Vec<String> = vec![bin.to_string()];
        want.extend(terminal.iter().map(|s| s.to_string()));
        want.extend(common().iter().map(|s| s.to_string()));
        want.extend(control.iter().cloned());
        want.push("studio.local".to_string());
        want.push(line.to_string());
        want
    }

    fn control_options(path: &Path) -> Vec<String> {
        [
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", path.display()),
            "-o".to_string(),
            "ControlPersist=60".to_string(),
        ]
        .into()
    }

    use std::path::Path;

    /// The socket chosen for `name` with this `$TMPDIR` and the real `/tmp` as the last resort.
    fn chosen(paths: &Paths, name: &str, tmpdir: Option<&str>, uid: u32) -> PathBuf {
        control_choice(paths, name, tmpdir.map(OsStr::new), Path::new("/tmp"), uid).into_path()
    }

    #[test]
    fn a_command_has_no_terminal_and_no_input() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        let m = machine(None);
        let ssh = Ssh {
            bin: PathBuf::from("/fake/ssh"),
        };
        let got = command(&ssh, &paths, &m, Mode::Command, &["workspace", "list"]);
        let line = remote_line(&m, &["workspace", "list"]);
        let control = control_options(&control_path(&paths, "studio"));
        assert_eq!(
            argv(&got),
            expected("/fake/ssh", &["-T", "-n"], &control, &line)
        );
    }

    #[test]
    fn a_feed_has_no_terminal_but_keeps_its_input() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        let m = machine(None);
        let ssh = Ssh {
            bin: PathBuf::from("/fake/ssh"),
        };
        let got = command(&ssh, &paths, &m, Mode::Feed, &["machine", "feed"]);
        let line = remote_line(&m, &["machine", "feed"]);
        let control = control_options(&control_path(&paths, "studio"));
        assert_eq!(argv(&got), expected("/fake/ssh", &["-T"], &control, &line));
    }

    #[test]
    fn an_attach_has_a_terminal() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        let m = machine(None);
        let ssh = Ssh {
            bin: PathBuf::from("/fake/ssh"),
        };
        let got = command(
            &ssh,
            &paths,
            &m,
            Mode::Attach,
            &["machine", "attach", "%16"],
        );
        let line = remote_line(&m, &["machine", "attach", "%16"]);
        let control = control_options(&control_path(&paths, "studio"));
        assert_eq!(argv(&got), expected("/fake/ssh", &["-t"], &control, &line));
    }

    #[test]
    fn a_fresh_command_uses_no_control_socket() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        let m = machine(Some("~/bin/taw"));
        let ssh = Ssh {
            bin: PathBuf::from("/fake/ssh"),
        };
        let got = command(&ssh, &paths, &m, Mode::Fresh, &["--version"]);
        let line = remote_line(&m, &["--version"]);
        let control = vec!["-o".to_string(), "ControlPath=none".to_string()];
        assert_eq!(
            argv(&got),
            expected("/fake/ssh", &["-T", "-n"], &control, &line)
        );
        let joined = argv(&got).join(" ");
        assert!(!joined.contains("ControlMaster") && !joined.contains("ControlPersist"));
    }

    #[test]
    fn askpass_is_never_asked() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        let m = machine(None);
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        for mode in [Mode::Feed, Mode::Command, Mode::Attach, Mode::Fresh] {
            let got = command(&ssh, &paths, &m, mode, &["x"]);
            assert_eq!(
                env_of(&got, "SSH_ASKPASS_REQUIRE").as_deref(),
                Some("never"),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_destination_is_one_element_and_never_an_option() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        let mut m = machine(None);
        m.ssh = "-oProxyCommand=x".to_string();
        let caught = std::panic::catch_unwind(|| {
            command(&ssh, &paths, &m, Mode::Command, &["x"]);
        });
        assert!(
            caught.is_err(),
            "a destination starting with - was built into a command"
        );
    }

    #[test]
    fn the_control_directory_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(dir.path().join("config"), dir.path().join("state"));
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        let m = machine(None);
        command(&ssh, &paths, &m, Mode::Command, &["x"]);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.control_dir()), 0o700);
        std::fs::set_permissions(paths.control_dir(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        command(&ssh, &paths, &m, Mode::Command, &["x"]);
        assert_eq!(
            mode(&paths.control_dir()),
            0o700,
            "an existing folder is made private too"
        );
    }

    #[test]
    fn a_fresh_command_makes_nothing_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(dir.path().join("config"), dir.path().join("state"));
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        command(&ssh, &paths, &machine(None), Mode::Fresh, &["--version"]);
        assert!(!paths.control_dir().exists());
        assert!(!paths.state_dir.exists());
    }

    #[test]
    fn the_control_path_is_in_the_state_folder_when_it_fits() {
        let (paths, _dir) = paths_in("/tmp/taw-state-a");
        assert_eq!(
            control_path(&paths, "studio"),
            PathBuf::from("/tmp/taw-state-a/m/studio")
        );
    }

    /// A state folder that puts `<state>/m/studio` at exactly `len` bytes.
    fn state_for(len: usize) -> String {
        let fixed = "/m/studio".len();
        format!("/{}", "s".repeat(len - fixed - 1))
    }

    #[test]
    fn a_control_path_of_86_bytes_stays_and_one_of_87_moves_to_tmpdir() {
        let at_86 = state_for(86);
        let (paths, _dir) = paths_in(&at_86);
        let got = chosen(&paths, "studio", Some("/var/t"), 501);
        assert_eq!(got.as_os_str().len(), 86);
        assert_eq!(got, PathBuf::from(format!("{at_86}/m/studio")));

        let at_87 = state_for(87);
        let (paths, _dir) = paths_in(&at_87);
        let got = chosen(&paths, "studio", Some("/var/t"), 501);
        let text = got.display().to_string();
        assert!(text.starts_with("/var/t/taw-501-"), "{text}");
        let hex = text.strip_prefix("/var/t/taw-501-").unwrap();
        assert_eq!(hex.len(), 8);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{hex}"
        );
    }

    #[test]
    fn the_tmpdir_name_is_the_hash_of_the_state_folder_a_nul_and_the_name() {
        use sha2::{Digest, Sha256};
        let state = state_for(90);
        let (paths, _dir) = paths_in(&state);
        let mut hasher = Sha256::new();
        hasher.update(state.as_bytes());
        hasher.update([0u8]);
        hasher.update(b"studio");
        let digest = hasher.finalize();
        let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            chosen(&paths, "studio", Some("/var/t"), 7),
            PathBuf::from(format!("/var/t/taw-7-{hex}"))
        );
        // Another machine, or another state folder, is another socket.
        assert_ne!(
            chosen(&paths, "pi", Some("/var/t"), 7),
            chosen(&paths, "studio", Some("/var/t"), 7)
        );
    }

    #[test]
    fn a_state_folder_with_a_space_a_percent_or_a_quote_moves_to_tmpdir() {
        for state in ["/tmp/my state", "/tmp/50%", "/tmp/a\"b"] {
            let (paths, _dir) = paths_in(state);
            let got = chosen(&paths, "studio", Some("/var/t"), 501);
            assert!(got.starts_with("/var/t"), "{state}: {}", got.display());
        }
    }

    #[test]
    fn tmpdir_unset_or_unsafe_gives_slash_tmp() {
        let (paths, _dir) = paths_in("/tmp/my state");
        for tmpdir in [None, Some("/var/my t"), Some("/var/50%"), Some("/var/a\"b")] {
            let got = chosen(&paths, "studio", tmpdir, 501);
            assert!(
                got.display().to_string().starts_with("/tmp/taw-501/"),
                "{tmpdir:?}: {}",
                got.display()
            );
        }
        let long = format!("/{}", "t".repeat(90));
        let got = chosen(&paths, "studio", Some(&long), 501);
        assert!(
            got.display().to_string().starts_with("/tmp/taw-501/"),
            "{}",
            got.display()
        );
    }

    #[test]
    fn a_state_folder_that_ssh_or_a_shell_would_misread_moves_to_tmpdir() {
        for state in [
            "/tmp/o'neil",
            "/tmp/a\\b",
            "/tmp/a\tb",
            "/tmp/a\nb",
            "/tmp/a$b",
            "/tmp/~x",
            "/tmp/a=b",
            "/tmp/a(b",
            "/tmp/a;b",
            "/tmp/caf\u{e9}",
            "relative/state",
        ] {
            let (paths, _dir) = paths_in(state);
            let got = chosen(&paths, "studio", Some("/var/t"), 501);
            assert!(got.starts_with("/var/t"), "{state:?}: {}", got.display());
        }
        // Not text at all.
        use std::os::unix::ffi::OsStringExt;
        let bad = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/a\xffb".to_vec()));
        let paths = Paths::rooted_at(PathBuf::from("/c"), bad);
        let got = chosen(&paths, "studio", Some("/var/t"), 501);
        assert!(got.starts_with("/var/t"), "{}", got.display());
    }

    #[test]
    fn a_path_of_only_safe_bytes_stays() {
        for state in ["/tmp/taw-state-a", "/tmp/a._+,:@-b/C9"] {
            let (paths, _dir) = paths_in(state);
            assert_eq!(
                chosen(&paths, "studio", Some("/var/t"), 501),
                PathBuf::from(format!("{state}/m/studio"))
            );
        }
    }

    #[test]
    fn a_tmpdir_that_ssh_or_a_shell_would_misread_is_not_used() {
        let (paths, _dir) = paths_in("/tmp/my state");
        for tmpdir in [
            "/var/o'neil",
            "/var/a\\b",
            "/var/a\tb",
            "/var/a\nb",
            "/var/a$b",
            "/var/~x",
            "/var/a=b",
            "var/relative",
        ] {
            let got = chosen(&paths, "studio", Some(tmpdir), 501);
            assert!(
                got.starts_with("/tmp/taw-501/"),
                "{tmpdir:?}: {}",
                got.display()
            );
        }
        let ok = chosen(&paths, "studio", Some("/var/folders/ab/T/"), 501);
        assert!(
            ok.display()
                .to_string()
                .starts_with("/var/folders/ab/T/taw-501-"),
            "{}",
            ok.display()
        );
    }

    /// A scratch folder standing for `/tmp`, and the places that use it with no `$TMPDIR`.
    fn scratch_places() -> (tempfile::TempDir, Places) {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("tmp");
        std::fs::create_dir(&tmp).unwrap();
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let places = Places {
            tmpdir: None,
            tmp,
            uid,
        };
        (dir, places)
    }

    /// A folder under `dir` with `mode`, whatever the umask says.
    fn folder_with_mode(dir: &Path, name: &str, mode: u32) -> std::ffi::OsString {
        use std::os::unix::fs::PermissionsExt;
        let folder = dir.join(name);
        std::fs::create_dir(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode)).unwrap();
        folder.into_os_string()
    }

    /// The control path a command takes for a state folder that is too long, with `tmpdir` as the
    /// runner's `$TMPDIR`, put through the same look at it that `command` makes.
    fn taken_with_tmpdir(places: &Places, tmpdir: &std::ffi::OsString) -> PathBuf {
        let (paths, _dir) = paths_in(&format!("/tmp/{}", "a".repeat(90)));
        let places = Places {
            tmpdir: shared_tmpdir_left_out(Some(tmpdir.clone())),
            tmp: places.tmp.clone(),
            uid: places.uid,
        };
        prepared_control_path(&paths, "studio", &places).unwrap()
    }

    #[test]
    fn a_tmpdir_only_its_owner_can_write_in_is_used() {
        let (dir, places) = scratch_places();
        let tmpdir = folder_with_mode(dir.path(), "mine", 0o700);
        let got = taken_with_tmpdir(&places, &tmpdir);
        assert!(
            got.starts_with(Path::new(&tmpdir)),
            "{} is not in {tmpdir:?}",
            got.display()
        );
        assert!(!places.tmp.join(format!("taw-{}", places.uid)).exists());
    }

    #[test]
    fn a_world_writable_tmpdir_is_not_used_and_the_socket_goes_to_the_private_folder() {
        for (name, mode) in [("open", 0o777), ("sticky", 0o1777)] {
            let (dir, places) = scratch_places();
            let tmpdir = folder_with_mode(dir.path(), name, mode);
            let got = taken_with_tmpdir(&places, &tmpdir);
            let private = places.tmp.join(format!("taw-{}", places.uid));
            assert!(
                got.starts_with(&private),
                "{name}: {} is not in {}",
                got.display(),
                private.display()
            );
            assert_eq!(mode_of(&private), 0o700, "{name}");
        }
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn the_last_resort_is_a_folder_of_our_own_inside_the_shared_one() {
        let (paths, _dir) = paths_in("/tmp/my state");
        let got = chosen(&paths, "studio", None, 501);
        let text = got.display().to_string();
        let hex = text.strip_prefix("/tmp/taw-501/").expect(&text);
        assert_eq!(hex.len(), 8, "{text}");
    }

    #[test]
    fn the_private_folder_is_made_0700_and_accepted_again_when_it_is_ours() {
        let (_dir, places) = scratch_places();
        let folder = private_folder(&places.tmp, places.uid).unwrap();
        assert_eq!(folder, places.tmp.join(format!("taw-{}", places.uid)));
        assert_eq!(mode_of(&folder), 0o700);
        assert_eq!(private_folder(&places.tmp, places.uid).unwrap(), folder);
    }

    #[test]
    fn a_private_folder_that_is_a_link_a_file_open_to_others_or_not_ours_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, places) = scratch_places();
        let name = format!("taw-{}", places.uid);
        let folder = places.tmp.join(&name);

        // A link, even to a folder that is ours and private.
        let target = places.tmp.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &folder).unwrap();
        let why = private_folder(&places.tmp, places.uid).unwrap_err();
        assert!(why.contains("not a folder"), "{why}");
        std::fs::remove_file(&folder).unwrap();

        // A folder that others can enter.
        std::fs::create_dir(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let why = private_folder(&places.tmp, places.uid).unwrap_err();
        assert!(why.contains("mode 755"), "{why}");
        std::fs::remove_dir(&folder).unwrap();

        // A regular file.
        std::fs::write(&folder, "x").unwrap();
        let why = private_folder(&places.tmp, places.uid).unwrap_err();
        assert!(why.contains("not a folder"), "{why}");
        std::fs::remove_file(&folder).unwrap();

        // A folder that belongs to someone else: one named for another user, which this user made.
        let other = places.uid + 1;
        std::fs::create_dir(places.tmp.join(format!("taw-{other}"))).unwrap();
        std::fs::set_permissions(
            places.tmp.join(format!("taw-{other}")),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let why = private_folder(&places.tmp, other).unwrap_err();
        assert!(why.contains("another user's"), "{why}");
    }

    /// A machine whose state folder has a space in it, so that the socket cannot be under it.
    fn spaced(dir: &Path) -> Paths {
        Paths::rooted_at(dir.join("config"), dir.join("my state"))
    }

    fn control_words(command: &Command) -> String {
        argv(command)
            .iter()
            .filter(|word| word.starts_with("Control"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_command_whose_last_resort_folder_is_refused_shares_nothing() {
        let (dir, places) = scratch_places();
        let paths = spaced(dir.path());
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        // A regular file where the folder is to be.
        std::fs::write(places.tmp.join(format!("taw-{}", places.uid)), "x").unwrap();
        let got = command_in(&ssh, &paths, &machine(None), Mode::Command, &["x"], &places);
        assert_eq!(control_words(&got), "ControlPath=none");
    }

    #[test]
    fn the_warning_for_a_refused_folder_says_the_machine_the_folder_and_why() {
        let (dir, places) = scratch_places();
        let paths = spaced(dir.path());
        let folder = places.tmp.join(format!("taw-{}", places.uid));
        // A regular file where the folder is to be.
        std::fs::write(&folder, "x").unwrap();
        assert_eq!(
            checked_control_path(&paths, "studio", &places),
            Err(format!(
                "no shared ssh connection for studio: {}: is not a folder (a link or a file is there)",
                folder.display()
            ))
        );
        // A folder that others can enter.
        std::fs::remove_file(&folder).unwrap();
        folder_with_mode(&places.tmp, &format!("taw-{}", places.uid), 0o755);
        assert_eq!(
            checked_control_path(&paths, "pi", &places),
            Err(format!(
                "no shared ssh connection for pi: {}: has mode 755, not 700",
                folder.display()
            ))
        );
        // The positive half: a folder that is ours gives a path and no warning.
        std::fs::remove_dir(&folder).unwrap();
        let path = checked_control_path(&paths, "studio", &places).unwrap();
        assert!(path.starts_with(&folder), "{}", path.display());
    }

    #[test]
    fn a_command_whose_last_resort_folder_is_fine_uses_a_socket_in_it() {
        let (dir, places) = scratch_places();
        let paths = spaced(dir.path());
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        let got = command_in(&ssh, &paths, &machine(None), Mode::Command, &["x"], &places);
        let folder = places.tmp.join(format!("taw-{}", places.uid));
        let words = control_words(&got);
        assert!(
            words.starts_with(&format!(
                "ControlMaster=auto ControlPath={}/",
                folder.display()
            )),
            "{words}"
        );
        assert!(words.ends_with("ControlPersist=60"), "{words}");
        assert_eq!(mode_of(&folder), 0o700);
    }

    #[test]
    fn only_the_folder_that_holds_the_chosen_socket_is_made() {
        let ssh = Ssh {
            bin: PathBuf::from("ssh"),
        };
        // The state folder's socket: the other two folders are not made.
        let (dir, places) = scratch_places();
        let paths = Paths::rooted_at(dir.path().join("config"), dir.path().join("state"));
        command_in(&ssh, &paths, &machine(None), Mode::Command, &["x"], &places);
        assert!(paths.control_dir().is_dir());
        assert!(!places.tmp.join(format!("taw-{}", places.uid)).exists());

        // A socket under TMPDIR: neither the state's `m` folder nor the last resort one is made.
        let (dir, mut places) = scratch_places();
        let paths = spaced(dir.path());
        let tmpdir = dir.path().join("tmpdir");
        std::fs::create_dir(&tmpdir).unwrap();
        places.tmpdir = Some(tmpdir.clone().into_os_string());
        let got = command_in(&ssh, &paths, &machine(None), Mode::Command, &["x"], &places);
        let words = control_words(&got);
        assert!(
            words.contains(&format!("ControlPath={}/taw-", tmpdir.display())),
            "{words}"
        );
        assert!(
            !paths.control_dir().exists(),
            "the state's m folder was made"
        );
        assert!(!paths.state_dir.exists());
        assert!(!places.tmp.join(format!("taw-{}", places.uid)).exists());

        // The last resort: the state's `m` folder is not made.
        places.tmpdir = None;
        command_in(&ssh, &paths, &machine(None), Mode::Command, &["x"], &places);
        assert!(!paths.control_dir().exists());
        assert!(places.tmp.join(format!("taw-{}", places.uid)).is_dir());
    }

    #[test]
    fn the_remote_line_runs_the_finder_with_the_arguments_quoted() {
        let line = remote_line(&machine(None), &["workspace", "list"]);
        assert!(line.starts_with("sh -c '"), "{line}");
        assert!(line.ends_with("' taw 'workspace' 'list'"), "{line}");
        assert!(line.contains("command -v \"$t\""), "{line}");
        assert!(line.contains("exit 127"), "{line}");
    }

    #[test]
    fn a_taw_with_a_home_prefix_runs_from_that_machines_home() {
        let line = remote_line(&machine(Some("~/x/taw")), &["a"]);
        assert_eq!(
            line,
            r#"sh -c '[ -x "$HOME"/'\''x/taw'\'' ] && exec "$HOME"/'\''x/taw'\'' "$@"; echo "sh: taw: command not found" >&2; exit 127' taw 'a'"#
        );
    }

    #[test]
    fn a_taw_with_a_full_path_runs_from_there() {
        let line = remote_line(&machine(Some("/opt/my taw/taw")), &["a"]);
        assert_eq!(
            line,
            r#"sh -c '[ -x '\''/opt/my taw/taw'\'' ] && exec '\''/opt/my taw/taw'\'' "$@"; echo "sh: taw: command not found" >&2; exit 127' taw 'a'"#
        );
    }

    /// Runs `line` as the other machine's shell would, in `dir`, with its own `HOME` and `PATH`, and
    /// returns the arguments the program it found was run with.
    fn run_remote(line: &str, home: &Path, path: &str) -> (std::process::Output, Vec<String>) {
        run_remote_with(line, home, path, &[])
    }

    /// As `run_remote`, with each `(name, value)` of `env` set there. The shell starts in `home`, the
    /// test's own temporary folder, so a relative path in `env` points under it and nothing is made
    /// in the crate root.
    fn run_remote_with(
        line: &str,
        home: &Path,
        path: &str,
        env: &[(&str, &str)],
    ) -> (std::process::Output, Vec<String>) {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(line)
            .env_clear()
            .env("HOME", home)
            .env("PATH", path)
            .current_dir(home);
        for (name, value) in env {
            cmd.env(name, value);
        }
        let out = cmd.output().unwrap();
        let args = out
            .stdout
            .split(|b| *b == 0)
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>();
        // A trailing NUL leaves one empty piece at the end.
        let args = args[..args.len().saturating_sub(1)].to_vec();
        (out, args)
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        crate::test_exec::make_executable(path);
    }

    fn printer(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        write_script(&path, "printf '%s\\0' \"$@\"\n");
        path
    }

    const AWKWARD: [&str; 15] = [
        "plain",
        "two words",
        "$(touch pwned)",
        "it's",
        "line one\nline two",
        "a\\b `c` \"d\" *",
        "",
        "{a,b}",
        "x{1..3}",
        "{}",
        "=x",
        "a b{c,d}",
        "!",
        "^",
        ",",
    ];

    #[test]
    fn what_the_other_shell_reads_is_the_argv_that_was_given() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = printer(&tmp.path().join("bin"), "taw");
        let home = tempfile::tempdir().unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.parent().unwrap().display());
        let (out, got) = run_remote(&remote_line(&machine(None), &AWKWARD), home.path(), &path);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(got, AWKWARD);
        assert!(!tmp.path().join("pwned").exists());
        assert!(!home.path().join("pwned").exists());
    }

    #[test]
    fn the_same_holds_for_a_full_path_and_for_a_path_under_home() {
        let tmp = tempfile::tempdir().unwrap();
        let full = printer(tmp.path(), "taw with space");
        let (out, got) = run_remote(
            &remote_line(&machine(Some(full.to_str().unwrap())), &AWKWARD),
            tmp.path(),
            "/usr/bin:/bin",
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(got, AWKWARD);

        let home = tempfile::tempdir().unwrap();
        printer(&home.path().join("tools dir"), "taw");
        let (out, got) = run_remote(
            &remote_line(&machine(Some("~/tools dir/taw")), &AWKWARD),
            home.path(),
            "/usr/bin:/bin",
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(got, AWKWARD);
    }

    #[test]
    fn a_taw_path_holding_a_single_quote_runs_with_its_arguments_intact() {
        // Under the home of the far machine: `~/it's here/taw`.
        let home = tempfile::tempdir().unwrap();
        printer(&home.path().join("it's here"), "taw");
        let (out, got) = run_remote(
            &remote_line(&machine(Some("~/it's here/taw")), &AWKWARD),
            home.path(),
            "/usr/bin:/bin",
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(got, AWKWARD);

        // An absolute path with a quote and a space in it.
        let tmp = tempfile::tempdir().unwrap();
        let full = printer(&tmp.path().join("o'x dir"), "taw");
        let (out, got) = run_remote(
            &remote_line(&machine(Some(full.to_str().unwrap())), &AWKWARD),
            tmp.path(),
            "/usr/bin:/bin",
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(got, AWKWARD);
    }

    #[test]
    fn with_no_taw_on_the_path_the_finder_tries_the_places_in_order() {
        let home = tempfile::tempdir().unwrap();
        printer(&home.path().join(".cargo/bin"), "taw");
        // A taw in ~/.cargo/bin is found when ~/.local/bin has none.
        let (out, got) = run_remote(
            &remote_line(&machine(None), &["cargo"]),
            home.path(),
            "/usr/bin:/bin",
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(got, ["cargo"]);
        // And ~/.local/bin comes before it.
        let marker = home.path().join("which");
        write_script(
            &home.path().join(".local/bin/taw"),
            &format!(
                "echo local > '{}'\nprintf '%s\\0' \"$@\"\n",
                marker.display()
            ),
        );
        let (out, _) = run_remote(
            &remote_line(&machine(None), &["x"]),
            home.path(),
            "/usr/bin:/bin",
        );
        assert!(out.status.success());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "local\n");
    }

    /// A stand-in `taw` that prints `marker` and nothing else.
    fn marker_taw(path: &Path, marker: &str) {
        write_script(path, &format!("printf '%s\\0' '{marker}'\n"));
    }

    fn found(home: &Path, path: &str, xdg: Option<&str>) -> (Option<i32>, Vec<String>) {
        let env: Vec<(&str, &str)> = xdg.map(|x| ("XDG_STATE_HOME", x)).into_iter().collect();
        found_env(home, path, &env)
    }

    fn found_env(home: &Path, path: &str, env: &[(&str, &str)]) -> (Option<i32>, Vec<String>) {
        let (out, got) = run_remote_with(&remote_line(&machine(None), &["x"]), home, path, env);
        (out.status.code(), got)
    }

    const PLUGIN_BIN: &str = ".local/state/tmux-agent-wizard/bin/taw";

    #[test]
    fn the_finder_runs_the_taw_in_the_plugins_folder_under_home() {
        let home = tempfile::tempdir().unwrap();
        marker_taw(&home.path().join(PLUGIN_BIN), "plugin");
        assert_eq!(
            found(home.path(), "/usr/bin:/bin", None),
            (Some(0), vec!["plugin".to_string()])
        );
    }

    #[test]
    fn the_finder_runs_the_taw_in_the_plugins_folder_under_an_absolute_xdg_state_home() {
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        marker_taw(&xdg.path().join("tmux-agent-wizard/bin/taw"), "xdg");
        marker_taw(&home.path().join(PLUGIN_BIN), "home");
        let xdg_str = xdg.path().to_str().unwrap();
        assert_eq!(
            found(home.path(), "/usr/bin:/bin", Some(xdg_str)),
            (Some(0), vec!["xdg".to_string()])
        );
    }

    #[test]
    fn an_xdg_state_home_with_no_taw_in_it_leaves_the_one_under_home() {
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        marker_taw(&home.path().join(PLUGIN_BIN), "home");
        assert_eq!(
            found(
                home.path(),
                "/usr/bin:/bin",
                Some(xdg.path().to_str().unwrap())
            ),
            (Some(0), vec!["home".to_string()])
        );
    }

    #[test]
    fn the_plugins_taw_runs_before_an_old_one_in_local_bin_and_the_other_places() {
        let home = tempfile::tempdir().unwrap();
        marker_taw(&home.path().join(PLUGIN_BIN), "plugin");
        marker_taw(&home.path().join(".local/bin/taw"), "old local");
        marker_taw(&home.path().join(".cargo/bin/taw"), "old cargo");
        assert_eq!(
            found(home.path(), "/usr/bin:/bin", None),
            (Some(0), vec!["plugin".to_string()])
        );
    }

    #[test]
    fn a_relative_xdg_state_home_is_not_used() {
        let home = tempfile::tempdir().unwrap();
        // `rel` is resolved against the shell's own folder, which is the test's HOME: the stand-in
        // under it is what the relative value points at, and a real one is under `~/.local/bin`.
        let rel = home.path().join("rel");
        marker_taw(&rel.join("tmux-agent-wizard/bin/taw"), "relative");
        marker_taw(&home.path().join(".local/bin/taw"), "local");
        assert_eq!(
            found(home.path(), "/usr/bin:/bin", Some("rel")),
            (Some(0), vec!["local".to_string()])
        );
    }

    #[test]
    fn the_shell_there_starts_in_the_tests_own_folder() {
        // What keeps a relative path in a test from making anything in the crate root: the stand-in
        // prints the folder it runs in.
        let home = tempfile::tempdir().unwrap();
        write_script(
            &home.path().join(".local/bin/taw"),
            "printf '%s\\0' \"$(pwd -P)\"\n",
        );
        let (_, got) = run_remote(
            &remote_line(&machine(None), &["x"]),
            home.path(),
            "/usr/bin:/bin",
        );
        let here = std::fs::canonicalize(home.path()).unwrap();
        assert_eq!(got, [here.to_str().unwrap()]);
    }

    const TPM_HOME: &str = ".tmux/plugins/tmux-agent-wizard/bin/taw";
    const TPM_CONFIG: &str = ".config/tmux/plugins/tmux-agent-wizard/bin/taw";

    #[test]
    fn the_finder_runs_the_taw_in_the_tpm_folder_under_home_dot_tmux() {
        let home = tempfile::tempdir().unwrap();
        marker_taw(&home.path().join(TPM_HOME), "tpm home");
        assert_eq!(
            found(home.path(), "/usr/bin:/bin", None),
            (Some(0), vec!["tpm home".to_string()])
        );
    }

    #[test]
    fn the_finder_runs_the_taw_in_the_tpm_folder_under_an_absolute_xdg_config_home() {
        let home = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        marker_taw(
            &cfg.path().join("tmux/plugins/tmux-agent-wizard/bin/taw"),
            "tpm xdg",
        );
        let cfg_str = cfg.path().to_str().unwrap();
        assert_eq!(
            found_env(
                home.path(),
                "/usr/bin:/bin",
                &[("XDG_CONFIG_HOME", cfg_str)]
            ),
            (Some(0), vec!["tpm xdg".to_string()])
        );
    }

    #[test]
    fn the_finder_runs_the_taw_in_the_tpm_folder_under_home_dot_config() {
        let home = tempfile::tempdir().unwrap();
        marker_taw(&home.path().join(TPM_CONFIG), "tpm config");
        assert_eq!(
            found(home.path(), "/usr/bin:/bin", None),
            (Some(0), vec!["tpm config".to_string()])
        );
    }

    #[test]
    fn a_relative_xdg_config_home_is_not_used() {
        let home = tempfile::tempdir().unwrap();
        marker_taw(
            &home
                .path()
                .join("rel/tmux/plugins/tmux-agent-wizard/bin/taw"),
            "relative",
        );
        marker_taw(&home.path().join(".local/bin/taw"), "local");
        assert_eq!(
            found_env(home.path(), "/usr/bin:/bin", &[("XDG_CONFIG_HOME", "rel")]),
            (Some(0), vec!["local".to_string()])
        );
    }

    #[test]
    fn the_tpm_folders_are_tried_in_order_after_the_state_folder_and_before_local_bin() {
        // Both variables are absolute and name folders with no taw yet, so each entry under HOME
        // is its own place. Each stand-in made is the one that must run next.
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let env = [
            ("XDG_STATE_HOME", state.path().to_str().unwrap()),
            ("XDG_CONFIG_HOME", cfg.path().to_str().unwrap()),
        ];
        let steps = [
            (home.path().join(".local/bin/taw"), "local"),
            (home.path().join(TPM_CONFIG), "tpm config"),
            (
                cfg.path().join("tmux/plugins/tmux-agent-wizard/bin/taw"),
                "tpm xdg",
            ),
            (home.path().join(TPM_HOME), "tpm home"),
            (home.path().join(PLUGIN_BIN), "state link"),
            (state.path().join("tmux-agent-wizard/bin/taw"), "state xdg"),
        ];
        for (path, marker) in steps {
            marker_taw(&path, marker);
            assert_eq!(found_env(home.path(), "/usr/bin:/bin", &env).1, [marker]);
        }
    }

    #[test]
    fn a_path_with_a_space_and_a_glob_character_is_one_word_in_every_entry() {
        // A folder `x d*g` holds the state folder and the TPM folder; a sibling `x dog` and `x d g`
        // would match the unquoted words, and a folder with several matches would hand `exec` more
        // than one word.
        for entry in ["state", "tpm xdg"] {
            let home = tempfile::tempdir().unwrap();
            let odd = home.path().join("x d*g");
            for decoy in ["x dog", "x d g"] {
                marker_taw(
                    &home.path().join(decoy).join("tmux-agent-wizard/bin/taw"),
                    "decoy",
                );
                marker_taw(
                    &home
                        .path()
                        .join(decoy)
                        .join("tmux/plugins/tmux-agent-wizard/bin/taw"),
                    "decoy",
                );
            }
            let odd_str = odd.to_str().unwrap();
            let env = if entry == "state" {
                marker_taw(&odd.join("tmux-agent-wizard/bin/taw"), "odd");
                [("XDG_STATE_HOME", odd_str)]
            } else {
                marker_taw(&odd.join("tmux/plugins/tmux-agent-wizard/bin/taw"), "odd");
                [("XDG_CONFIG_HOME", odd_str)]
            };
            assert_eq!(
                found_env(home.path(), "/usr/bin:/bin", &env),
                (Some(0), vec!["odd".to_string()]),
                "{entry}"
            );
        }
    }

    #[test]
    fn a_home_with_a_space_and_a_glob_character_runs_each_entry_under_it() {
        // XDG_STATE_HOME and XDG_CONFIG_HOME are absolute and hold no taw, so the entries under
        // HOME are the ones that find it.
        for (rel, name) in [
            (PLUGIN_BIN, "state link"),
            (TPM_HOME, "tpm home"),
            (TPM_CONFIG, "tpm config"),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let none = tempfile::tempdir().unwrap();
            let none_str = none.path().to_str().unwrap();
            let home = parent.path().join("h o*me");
            for decoy in ["h ome", "h o me"] {
                marker_taw(&parent.path().join(decoy).join(rel), "decoy");
            }
            marker_taw(&home.join(rel), name);
            assert_eq!(
                found_env(
                    &home,
                    "/usr/bin:/bin",
                    &[("XDG_STATE_HOME", none_str), ("XDG_CONFIG_HOME", none_str)]
                ),
                (Some(0), vec![name.to_string()]),
                "{rel}"
            );
        }
    }

    #[test]
    fn taw_on_the_path_wins_over_the_plugins_folder() {
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        marker_taw(&home.path().join(PLUGIN_BIN), "home");
        marker_taw(&xdg.path().join("tmux-agent-wizard/bin/taw"), "xdg");
        let bin = tempfile::tempdir().unwrap();
        marker_taw(&bin.path().join("taw"), "path");
        let path = format!("{}:/usr/bin:/bin", bin.path().display());
        assert_eq!(
            found(home.path(), &path, Some(xdg.path().to_str().unwrap())),
            (Some(0), vec!["path".to_string()])
        );
    }

    #[test]
    fn a_home_with_no_taw_anywhere_exits_127() {
        // The finder also tries /opt/homebrew/bin and /usr/local/bin, which are not under the test's
        // HOME: a machine that has a taw in one of them (a developer's) runs that one instead.
        if ["/opt/homebrew/bin/taw", "/usr/local/bin/taw"]
            .iter()
            .any(|p| Path::new(p).exists())
        {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        assert_eq!(
            found(
                home.path(),
                "/usr/bin:/bin",
                Some(xdg.path().to_str().unwrap())
            )
            .0,
            Some(127)
        );
        assert_eq!(found(home.path(), "/usr/bin:/bin", None).0, Some(127));
    }

    #[test]
    fn a_taw_that_is_not_there_exits_127() {
        let home = tempfile::tempdir().unwrap();
        let (out, _) = run_remote(
            &remote_line(&machine(Some("~/nothing/taw")), &["x"]),
            home.path(),
            "/usr/bin:/bin",
        );
        assert_eq!(out.status.code(), Some(127));
    }

    #[test]
    fn the_last_line_is_the_last_one_that_says_something() {
        assert_eq!(
            last_line(
                b"Warning: x\nssh: connect to host studio.local port 22: Operation timed out\n\n"
            ),
            "ssh: connect to host studio.local port 22: Operation timed out"
        );
        assert_eq!(last_line(b""), "");
        assert_eq!(last_line(b"  \n\n"), "");
        assert_eq!(last_line(b"one\n  two  \n"), "two");
    }

    #[test]
    fn a_long_destination_does_not_hide_the_words_at_the_end_of_the_line() {
        use super::super::words::{Heard, Outcome, classify};
        let line = format!("{}: Permission denied (publickey).\n", "d".repeat(300));
        let said = last_line(line.as_bytes());
        assert!(said.chars().count() > 300);
        assert_eq!(classify(Some(255), &said, Heard::Nothing), Outcome::Login);
        let line = format!("{}: Host key verification failed.\n", "d".repeat(300));
        assert_eq!(
            classify(Some(255), &last_line(line.as_bytes()), Heard::Nothing),
            Outcome::HostKey
        );
        // What is shown is cut.
        assert_eq!(shown(&said).chars().count(), 256);
    }

    #[test]
    fn a_last_line_of_blanks_longer_than_the_cut_still_keeps_the_text_after_them() {
        let line = format!("{}ssh: refused\n", " ".repeat(300));
        assert_eq!(last_line(line.as_bytes()), "ssh: refused");
    }

    #[test]
    fn the_last_line_is_plain_text() {
        assert_eq!(
            last_line(b"bad \x1b[31mred\x1b[0m\x07 line\r\n"),
            "bad red line"
        );
        assert_eq!(last_line(&[b'o', b'k', 0xff, b'!', b'\n']), "ok\u{fffd}!");
    }

    #[test]
    fn a_whole_operating_system_command_is_taken_out_of_the_last_line() {
        assert_eq!(last_line(b"\x1b]0;title\x07ssh: refused\n"), "ssh: refused");
        assert_eq!(
            last_line(b"\x1b]8;;http://x\x1b\\ssh: refused\x1b]8;;\x1b\\\n"),
            "ssh: refused"
        );
        // One that never ends takes the rest of its line, and no more.
        assert_eq!(last_line(b"before\nssh \x1b]0;title\n"), "ssh");
        // A lone escape before another character still loses both, as it did.
        assert_eq!(last_line(b"a\x1bZb\n"), "ab");
    }

    #[test]
    fn the_program_is_taken_from_the_seam_variable_when_it_is_set_and_not_empty() {
        use std::ffi::OsString;
        assert_eq!(
            Ssh::from_var(Some(OsString::from("/stand/in"))).bin,
            PathBuf::from("/stand/in")
        );
        assert_eq!(
            Ssh::from_var(Some(OsString::new())).bin,
            PathBuf::from("ssh")
        );
        assert_eq!(Ssh::from_var(None).bin, PathBuf::from("ssh"));
    }
}
