//! What the tests of `taw open` and `taw ui open` share: a stand-in `tmux` that answers what
//! they ask of it and records every call, and the readers of what it recorded.

use std::path::{Path, PathBuf};

use super::Ctx;
use crate::config::Config;
use crate::tmux::Tmux;

/// What the stand-in `tmux` answers. Reads come back the way tmux 3.6b prints them, for a
/// 200x50 client (`/dev/ttys001`) at pane `%5` of session `$3`, with no `@agent-wizard-*`
/// options, no agent panes and no saved record; `tmux -V` reports `tmux 3.6b` unless `version` says another; everything
/// that changes something succeeds, except `display-popup`, which exits `popup_exit`, and a
/// `set-option` of the one option `refuses` names.
pub(crate) struct Stub {
    /// The `status` option, as `show-options -g` prints it: `on`, `off` or a row count.
    pub status: &'static str,
    /// How `display-popup` ends, and what it says on stderr when that is not 0.
    pub popup_exit: u8,
    pub popup_says: &'static str,
    /// A client is attached, to session `$0`, and `display-message` finds its size and its
    /// name. With none, `list-clients` prints nothing, the name is empty and the size is a
    /// lone space, as real tmux prints them with no client to ask.
    pub clients: bool,
    /// One session, `main` (`$3`, in /tmp), with one window, is running: what the bar
    /// counts. Without it tmux lists no sessions at all.
    pub running: bool,
    /// A client is attached to the session of pane `%5`, which is what tmux counts in
    /// `#{session_attached}`. Without it that reads 0: the pane is in a session nobody is
    /// looking at, whatever clients there are elsewhere.
    pub watched: bool,
    /// The stand-in keeps session options as a server does: what `set-option -t` sets,
    /// `show-options -v -t` reads back and `set-option -u -t` takes away (the whole of
    /// `status-format` with its rows), and `show-options -t <session> status-format` lists the
    /// rows the session holds. They are files in `options/` beside the log, and `display-popup`
    /// lists them into `options-at-popup`, so a test can say what the session held while the
    /// popup was open. Without it every read finds nothing, whatever was set.
    pub keeps_options: bool,
    /// A `set-option -t` of this option fails, as tmux fails one for a session it can no
    /// longer find, and sets nothing.
    pub refuses: Option<&'static str>,
    /// The client's own process, as `#{client_pid}` reads it (the pane is `%5`): 0 is none.
    pub client_pid: u32,
    /// `has-session` finds no session, as tmux says of one that went.
    pub session_gone: bool,
    /// The tty `list-clients` gives the client, when it is not the default `/dev/ttys001`: a test that
    /// delivers to terminals names a file of its own, since what is listed is opened and written to.
    pub client_tty: Option<PathBuf>,
    /// What `tmux -V` prints.
    pub version: &'static str,
}

impl Default for Stub {
    fn default() -> Stub {
        Stub {
            status: "on",
            popup_exit: 0,
            popup_says: "no current client",
            clients: true,
            running: true,
            watched: true,
            keeps_options: false,
            refuses: None,
            client_pid: 0,
            session_gone: false,
            client_tty: None,
            version: "tmux 3.6b",
        }
    }
}

/// The stand-in `tmux` in `dir`, and the log its calls are appended to: one line per call,
/// each argument followed by `\x1f`. `Tmux::run` puts `-u` ahead of its arguments, and the
/// stand-in drops it before it looks or logs, so a call reads as its verb and what follows.
pub(crate) fn stub_tmux(dir: &Path, stub: &Stub) -> (Tmux, PathBuf) {
    let log = dir.join("argv.log");
    let script = dir.join("tmux");
    let client = if let Some(tty) = stub.client_tty.as_ref().filter(|_| stub.clients) {
        format!(r"printf '{0}\t{0}\t$0\tmain\n'", tty.display())
    } else if stub.clients {
        r"printf '/dev/ttys001\t/dev/ttys001\t$0\tmain\n'".to_string()
    } else {
        ":".to_string()
    };
    let dims = if stub.clients { "200 50" } else { " " };
    // The client tmux takes as the current one: its name, or nothing with none attached.
    let name = if stub.clients {
        r"echo /dev/ttys001"
    } else {
        ":"
    };
    let attached = u8::from(stub.watched);
    let (sessions, windows) = if stub.running {
        (
            r"printf '$3\tmain\t/tmp\t1\t1\t1700000000\t\n'",
            r"printf '@1\t$3\t0\tzsh\t1\t1\tb25f,200x49,0,0,1\n'",
        )
    } else {
        (":", ":")
    };
    // A user option nobody set is an error to `show-options -v`; any other reads empty.
    let unset_read = r#"case "$5" in @*) echo "invalid option: $5" >&2; exit 1 ;; esac"#;
    let store = dir.join("options");
    let (read, rows, keep, drop, at_popup) = if stub.keeps_options {
        std::fs::create_dir_all(&store).unwrap();
        let s = store.display();
        (
            format!(r#"if [ -f '{s}/'"$5" ]; then cat '{s}/'"$5"; echo; else {unset_read}; fi"#),
            format!(
                r#"for f in '{s}'/status-format\[*; do [ -f "$f" ] && printf '%s %s\n' "${{f##*/}}" "$(cat "$f")"; done"#
            ),
            format!(r#"printf '%s' "$5" > '{s}/'"$4""#),
            format!(
                r#"case "$5" in status-format) rm -f '{s}'/status-format\[* ;; *) rm -f '{s}/'"$5" ;; esac"#
            ),
            format!("ls '{s}' > '{}'", dir.join("options-at-popup").display()),
        )
    } else {
        (
            unset_read.to_string(),
            ":".to_string(),
            ":".to_string(),
            ":".to_string(),
            ":".to_string(),
        )
    };
    let refuse = match stub.refuses {
        Some(name) => {
            format!(r#"[ "$4" = '{name}' ] && {{ echo "can't find session: $3" >&2; exit 1; }}"#)
        }
        None => ":".to_string(),
    };
    let body = format!(
        r#"#!/bin/sh
[ "$1" = -u ] && shift
{{ for a in "$@"; do printf '%s\037' "$a"; done; printf '\n'; }} >> '{log}'
case "$1" in
  -V) echo '{version}' ;;
  list-clients) {client} ;;
  display-message)
    for last; do :; done
    case "$last" in
      *client_width*) echo '{dims}' ;;
      *client_pid*) echo '{client_pid} %5' ;;
      '#{{client_name}}') {name} ;;
      '#{{client_last_session}}') echo main ;;
      '#{{client_theme}}') echo dark ;;
      '#{{pane_id}}') echo '%5' ;;
      '#{{session_id}}') echo '$3' ;;
      '#{{session_id}} #{{window_id}}') echo '$3 @1' ;;
      '#{{session_attached}}') echo '{attached}' ;;
    esac ;;
  show-options)
    case "$2" in
      -g) printf 'status {status}\nstatus-position bottom\n' ;;
      -v) {read} ;;
      -t) {rows} ;;
    esac ;;
  set-option)
    case "$2" in
      -t) {refuse}; {keep} ;;
      -u) [ "$3" = -t ] && {drop} ;;
    esac ;;
  has-session) exit {has_session} ;;
  list-sessions) {sessions} ;;
  list-windows) {windows} ;;
  display-popup) {at_popup}; [ {exit} -eq 0 ] || {{ echo '{says}' >&2; exit {exit}; }} ;;
esac
exit 0
"#,
        log = log.display(),
        status = stub.status,
        version = stub.version,
        exit = stub.popup_exit,
        says = stub.popup_says,
        client_pid = stub.client_pid,
        has_session = u8::from(stub.session_gone),
    );
    std::fs::write(&script, body).unwrap();
    crate::test_exec::make_executable(&script);
    (
        Tmux {
            bin: script,
            socket: None,
        },
        log,
    )
}

/// A context over `tmux` with `zoxide add` off: on, it would run the real one, on a path
/// in a temp directory.
///
/// Its folders are in the stand-in's own (`config` and `state` beside its `tmux`), so what a verb
/// reads or writes there, an app's record or the events file, is the test's and no one else's.
pub(crate) fn stub_ctx(tmux: Tmux) -> Ctx {
    let mut cfg = Config::default();
    cfg.sessions.zoxide = false;
    let home = tmux
        .bin
        .parent()
        .map_or_else(std::env::temp_dir, Path::to_path_buf);
    Ctx {
        paths: crate::paths::Paths::rooted_at(home.join("config"), home.join("state")),
        tmux,
        cfg,
        taw_bin: PathBuf::from("/usr/local/bin/taw"),
    }
}

/// Every call the stand-in was run with, in order, as its arguments.
pub(crate) fn calls(log: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let line = line.strip_suffix('\u{1f}').unwrap_or(line);
            line.split('\u{1f}').map(str::to_string).collect()
        })
        .collect()
}

/// The calls whose verb is one of `verbs`: what was done to the server, without the reads.
pub(crate) fn changes(log: &Path, verbs: &[&str]) -> Vec<Vec<String>> {
    calls(log)
        .into_iter()
        .filter(|argv| verbs.contains(&argv[0].as_str()))
        .collect()
}

/// Ends the throwaway server `t` names, waits for it to be gone and removes `path`, the file it said
/// it listens on (`#{socket_path}`): tmux leaves that file behind when a server ends, one for every
/// server a run of the tests started. A path that is not a socket file, whose name is not a test
/// server's (`taw-...`) or is not the file of the server `t` names is left alone, and so is the file
/// of a server that is still there ten seconds after it was told to end. The same as
/// `tests/common/shell_env.rs` has for the integration tests, which a unit test cannot include.
pub(crate) fn take_the_socket_away(t: &Tmux, path: &Path) {
    use crate::tmux::Socket;
    use std::os::unix::fs::FileTypeExt;
    use std::time::{Duration, Instant};

    let name_is_a_test_servers = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("taw-") && name != "default");
    let is_a_socket =
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket());
    let is_the_file_of_t = match &t.socket {
        Some(Socket::Name(name)) => path.file_name().is_some_and(|file| file == name.as_str()),
        Some(Socket::Path(own)) => own == path,
        None => false,
    };
    if !(name_is_a_test_servers && is_a_socket && is_the_file_of_t) {
        return;
    }
    // `list-sessions` starts nothing: it fails once the server is gone, which is already so for a
    // fixture that ended its server before it came here.
    if t.run(&["list-sessions"]).is_ok() {
        let _ = t.run_ok(&["kill-server"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while t.run(&["list-sessions"]).is_ok() {
            if Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    let _ = std::fs::remove_file(path);
}
