//! The pane's report folder: a folder on this machine that a profile mounts into
//! its sandbox, where the agent's taw hook writes the conversation's id as one file, which taw reads
//! from outside. One per pane, under taw's state, `<state dir>/sandbox/panes/<server>/<pane>/`, and
//! emptied when taw starts an agent in the pane and when the agent leaves.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::paths::Paths;
use crate::tmux::Tmux;

/// The report's file in the folder: the default profile names it `/run/taw/session.json` inside.
pub const FILE: &str = "session.json";

/// The tmux server's name, which the folders are kept by ([`key_of`] its socket).
pub fn server_key(t: &Tmux) -> anyhow::Result<String> {
    Ok(key_of(&t.run(&[
        "display-message",
        "-p",
        "#{socket_path}",
    ])?))
}

/// What tmux says of the pane `taw sandbox` was typed in: its id, its terminal, and the name of its
/// server that the report folders are kept by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub id: String,
    pub tty: PathBuf,
    pub server: String,
}

/// Asks tmux about `pane`, a pane id in tmux's form, in one call and before anything is made of it:
/// its id, its terminal and its server's socket. `None` when tmux knows no such pane: it answers for none (tmux
/// 3.6b prints an empty id for a pane it does not know), or for another.
pub fn ask(t: &Tmux, pane: &str) -> anyhow::Result<Option<Pane>> {
    let answer = t.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_id} #{pane_tty} #{socket_path}",
    ])?;
    Ok(answer_of(&answer).filter(|known| known.id == pane))
}

/// An answer of [`ask`]'s read: the pane's id, its terminal and its server's name, from `%31
/// /dev/ttys012 /private/tmp/tmux-501/default` (a socket's path can hold a space, and comes last).
fn answer_of(answer: &str) -> Option<Pane> {
    let (id, rest) = answer.split_once(' ')?;
    let (tty, socket) = rest.split_once(' ')?;
    (crate::sandbox::start::is_pane_id(id) && tty.starts_with('/') && !socket.is_empty()).then(
        || Pane {
            id: id.to_string(),
            tty: PathBuf::from(tty),
            server: key_of(socket),
        },
    )
}

/// The name the report folders of the tmux server listening on `socket` are kept by: the socket's
/// file name, anything a folder's name should not hold made `_`, then a dash and a hash of the
/// socket's whole path. Two servers whose sockets share a file name in two folders (the `default`
/// of two `TMUX_TMPDIR`s, two `-S` sockets of one name) are two servers, and a pane of one is not
/// the pane of the same id on the other: their folders are apart. One plain name either way, so
/// that [`folder`] takes it. `default` for no socket at all.
pub fn key_of(socket: &str) -> String {
    let socket = socket.trim();
    if socket.is_empty() {
        return "default".to_string();
    }
    let name: String = Path::new(socket)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}-{:016x}", fnv1a(socket.as_bytes()))
}

/// FNV-1a, in 64 bits: the same for the same bytes whatever taw computes it, and whichever Rust
/// built that taw. The start of a sandbox and the watcher of its pane each name the pane's folder,
/// and either may be the older taw.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The report folder of `pane` on the server `server`, `None` unless both are one plain name: a
/// folder's name taken from elsewhere must neither climb out of taw's state nor be a path of its own.
pub fn folder(paths: &Paths, server: &str, pane: &str) -> Option<PathBuf> {
    (one_name(server) && one_name(pane)).then(|| {
        crate::sandbox::profile::state_dir(paths)
            .join("panes")
            .join(server)
            .join(pane)
    })
}

/// Whether `name` is one plain name of a folder: not empty, neither `.` nor `..`, and no `/` or NUL
/// in it.
fn one_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// Makes `folder`, yours alone, and takes away everything in it, a link as a link: what the last
/// agent's sandbox left there is no report of the next one's. A link where the folder goes is
/// refused, so that nothing is made or emptied in what it leads to.
pub fn empty(folder: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(folder).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::other(format!(
            "{} is a link, where taw keeps a folder of its own",
            folder.display()
        )));
    }
    std::fs::create_dir_all(folder)?;
    std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))?;
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The most a report may be: one JSON object of at most 64 KiB.
pub const MOST: u64 = 64 * 1024;

/// Which file a report was: one that changed, or another put in its place, is another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
}

/// A report file that was read: which file it was, and what it held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub id: FileId,
    pub bytes: Vec<u8>,
}

/// Why a report was not read, or not believed: what the watcher's log says, and never what the
/// file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Ignored {
    #[error("it is a symbolic link")]
    Link,
    #[error("it is not a regular file")]
    NotAFile,
    #[error("it is over 64 KiB")]
    TooBig,
    #[error("it could not be read")]
    Unreadable,
    #[error("it is not one JSON object")]
    NotJson,
    #[error("it is another agent's")]
    OtherAgent,
    #[error("it is a sub-agent's")]
    SubAgent,
    #[error("it is not the start of a session")]
    NotSessionStart,
    #[error("it names no session")]
    NoSession,
    #[error("its session id is not one taw keeps")]
    BadId,
    #[error("its session path is not one taw keeps")]
    BadPath,
    #[error("its source is not one taw keeps")]
    BadSource,
    #[error("taw reads no report of this agent's")]
    NoRule,
}

/// The report in `folder` ([`FILE`]): opened without following a symbolic link and without
/// blocking, and read only when it is a regular file of at most [`MOST`] bytes. `Ok(None)` when
/// there is none.
pub fn read(folder: &Path) -> Result<Option<Found>, Ignored> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(folder.join(FILE));
    let file = match opened {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => return Err(Ignored::Link),
        Err(_) => return Err(Ignored::Unreadable),
    };
    let meta = file.metadata().map_err(|_| Ignored::Unreadable)?;
    if !meta.file_type().is_file() {
        return Err(Ignored::NotAFile);
    }
    if meta.len() > MOST {
        return Err(Ignored::TooBig);
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MOST + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Ignored::Unreadable)?;
    // It grew between the look and the read.
    if bytes.len() as u64 > MOST {
        return Err(Ignored::TooBig);
    }
    let id = FileId {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
        modified: (meta.mtime(), meta.mtime_nsec()),
    };
    Ok(Some(Found { id, bytes }))
}

/// What one look at a pane's report found, so the watcher takes a report once each time it changes
/// and says why it did not take one once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Nothing,
    File(FileId),
    Refused(Ignored),
}

/// The key a report an integration made itself names its agent with.
pub const MADE_BY: &str = "taw_agent";

/// What a report says, once taw believes it: the session's id and the path of its file, each where
/// the agent's hook keeps it, and what started the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reported {
    pub id: Option<String>,
    pub path: Option<String>,
    pub source: Option<String>,
}

/// What the report `bytes` says for `agent` (the agent the container in front of the pane runs),
/// under the rules that agent's hook applies outside a sandbox: the container can say one thing,
/// and taw decides whether to believe it.
pub fn session_of(agent: &str, bytes: &[u8]) -> Result<Reported, Ignored> {
    let Ok(Value::Object(payload)) = serde_json::from_slice::<Value>(bytes) else {
        return Err(Ignored::NotJson);
    };
    let reported = match payload.get(MADE_BY) {
        Some(made_by) if made_by.as_str() == Some(agent) => made(&payload)?,
        Some(_) => return Err(Ignored::OtherAgent),
        None => match agent {
            "claude" => claude(&payload)?,
            "codex" => codex(&payload)?,
            "copilot" => copilot(&payload)?,
            "letta" => letta(&payload)?,
            other => read_plain(&plain(other).ok_or(Ignored::NoRule)?, &payload)?,
        },
    };
    checked(reported)
}

/// A report an integration made itself: a SessionStart with its session id, the path of its
/// session file where it keeps one, and what started it.
fn made(payload: &Map<String, Value>) -> Result<Reported, Ignored> {
    if text(payload, "hook_event_name") != Some("SessionStart") {
        return Err(Ignored::NotSessionStart);
    }
    Ok(Reported {
        id: text(payload, "session_id").map(str::to_string),
        path: text(payload, "transcript_path").map(str::to_string),
        source: text(payload, "source").map(str::to_string),
    })
}

/// Claude's SessionStart input, read as `taw-agent-state-claude.sh` reads it.
fn claude(payload: &Map<String, Value>) -> Result<Reported, Ignored> {
    if payload.get("agent_id").is_some_and(truthy) {
        return Err(Ignored::SubAgent);
    }
    // Cursor embeds Claude Code and says so: that run is not a plain claude session.
    if payload.contains_key("cursor_version") {
        return Err(Ignored::OtherAgent);
    }
    if text(payload, "hook_event_name") != Some("SessionStart") {
        return Err(Ignored::NotSessionStart);
    }
    let id = text(payload, "session_id").ok_or(Ignored::NoSession)?;
    Ok(Reported {
        id: Some(id.to_string()),
        path: text(payload, "transcript_path").map(str::to_string),
        source: text(payload, "source").map(str::to_string),
    })
}

/// Codex's SessionStart input, read as `taw-agent-state-codex.sh` reads it.
fn codex(payload: &Map<String, Value>) -> Result<Reported, Ignored> {
    if payload.get("agent_id").is_some_and(truthy) {
        return Err(Ignored::SubAgent);
    }
    // Its hook reads the event name as `str(value or "")`: a falsy value is no event, any other
    // is an event, and only the text SessionStart is the right one.
    let started = match payload.get("hook_event_name").filter(|value| truthy(value)) {
        None => false,
        Some(Value::String(event)) if event == "SessionStart" => true,
        Some(_) => return Err(Ignored::NotSessionStart),
    };
    let id = text(payload, "session_id").ok_or(Ignored::NoSession)?;
    // herdr's evidence that this is a real start: a transcript path, which is not kept.
    let evidence = payload.get("transcript_path").and_then(Value::as_str);
    if evidence.is_none_or(|path| path.trim().is_empty()) {
        return Err(Ignored::NotSessionStart);
    }
    Ok(Reported {
        id: Some(id.to_string()),
        path: None,
        source: started
            .then(|| text(payload, "source").map(str::to_string))
            .flatten(),
    })
}

/// The rules of a shell hook that reads its input plainly: the events it takes, where its id and
/// path are, and what started the session.
struct Plain {
    /// The event names it takes (`hook_event_name`, or `hookEventName`); `None` among them takes a
    /// report that names none. Empty: it looks at no event name.
    events: &'static [Option<&'static str>],
    id: &'static [&'static str],
    path: &'static [&'static str],
    source: Source,
}

/// Where a plain hook's report says what started the session.
enum Source {
    None,
    /// Always this word (Kimi's and Mastra Code's `startup`).
    Fixed(&'static str),
    /// The `source` field, when it is one of these; any, when they are none.
    Field(&'static [&'static str]),
}

/// The rules of `agent`'s shell hook, each as its template reads its input outside a sandbox.
fn plain(agent: &str) -> Option<Plain> {
    let none = Source::None;
    Some(match agent {
        "cursor" => Plain {
            events: &[None, Some("sessionStart")],
            id: &[
                "session_id",
                "sessionId",
                "conversation_id",
                "conversationId",
            ],
            path: &[],
            source: none,
        },
        "devin" => Plain {
            events: &[],
            id: &["session_id", "sessionId"],
            path: &[],
            source: none,
        },
        "droid" | "qodercli" => Plain {
            events: &[],
            id: &["session_id"],
            path: &[],
            source: none,
        },
        "grok" => Plain {
            events: &[
                None,
                Some("session_start"),
                Some("SessionStart"),
                Some("sessionStart"),
            ],
            id: &["session_id", "sessionId"],
            path: &[],
            source: Source::Field(&[]),
        },
        "kimi" | "mastracode" => Plain {
            events: &[],
            id: &["session_id"],
            path: &[],
            source: Source::Fixed("startup"),
        },
        "agy" => Plain {
            events: &[],
            id: &["conversationId"],
            path: &["transcriptPath"],
            source: none,
        },
        "qwen" => Plain {
            events: &[],
            id: &["session_id"],
            path: &[],
            source: Source::Field(&["startup", "resume", "clear", "compact", "branch"]),
        },
        _ => return None,
    })
}

fn read_plain(rule: &Plain, payload: &Map<String, Value>) -> Result<Reported, Ignored> {
    if !rule.events.is_empty() {
        let event = text(payload, "hook_event_name").or_else(|| text(payload, "hookEventName"));
        if !rule.events.contains(&event) {
            return Err(Ignored::NotSessionStart);
        }
    }
    let id = rule
        .id
        .iter()
        .find_map(|key| text(payload, key))
        .ok_or(Ignored::NoSession)?;
    let source = match rule.source {
        Source::None => None,
        Source::Fixed(word) => Some(word),
        Source::Field(allowed) => {
            text(payload, "source").filter(|word| allowed.is_empty() || allowed.contains(word))
        }
    };
    Ok(Reported {
        id: Some(id.to_string()),
        path: rule
            .path
            .iter()
            .find_map(|key| text(payload, key))
            .map(str::to_string),
        source: source.map(str::to_string),
    })
}

/// Copilot's SessionStart input, read as `taw-agent-state-copilot.sh` reads it.
fn copilot(payload: &Map<String, Value>) -> Result<Reported, Ignored> {
    if payload.get("agent_id").is_some_and(truthy) {
        return Err(Ignored::SubAgent);
    }
    let squashed = |event: &str| event.replace(['_', '-'], "").to_lowercase();
    match text(payload, "hook_event_name").or_else(|| text(payload, "hookEventName")) {
        Some(event) if squashed(event) != "sessionstart" => return Err(Ignored::NotSessionStart),
        Some(_) => {}
        None => {
            let other = [
                "tool_name",
                "toolName",
                "notification_type",
                "notificationType",
                "stop_reason",
                "stopReason",
                "reason",
            ];
            if payload.contains_key("prompt")
                || other.iter().any(|key| text(payload, key).is_some())
            {
                return Err(Ignored::NotSessionStart);
            }
        }
    }
    let id = text(payload, "session_id")
        .or_else(|| text(payload, "sessionId"))
        .ok_or(Ignored::NoSession)?;
    Ok(Reported {
        id: Some(id.to_string()),
        path: None,
        source: None,
    })
}

/// Letta's SessionStart input, read as `taw-agent-session-letta.sh` reads it: its default
/// conversation, which has no id of its own, as `default:<agent_id>` (herdr's encoding).
fn letta(payload: &Map<String, Value>) -> Result<Reported, Ignored> {
    let conversation = text(payload, "conversation_id").ok_or(Ignored::NoSession)?;
    let id = match conversation {
        "default" => format!(
            "default:{}",
            text(payload, "agent_id").ok_or(Ignored::NoSession)?
        ),
        other => other.to_string(),
    };
    let source = if payload.get("is_new_session").is_some_and(truthy) {
        "new"
    } else {
        "resume"
    };
    Ok(Reported {
        id: Some(id),
        path: None,
        source: Some(source.to_string()),
    })
}

/// What every report passes, whoever made it: it names a session, by an id or a path, and each is
/// one taw keeps (the checks `pane report-agent-session` and restore make).
fn checked(reported: Reported) -> Result<Reported, Ignored> {
    if reported.id.is_none() && reported.path.is_none() {
        return Err(Ignored::NoSession);
    }
    if reported
        .id
        .as_deref()
        .is_some_and(|id| !crate::session::restore::valid_session_id(id))
    {
        return Err(Ignored::BadId);
    }
    if reported
        .path
        .as_deref()
        .is_some_and(|path| !crate::session::restore::valid_session_path(path))
    {
        return Err(Ignored::BadPath);
    }
    if reported
        .source
        .as_deref()
        .is_some_and(|source| !valid_source(source))
    {
        return Err(Ignored::BadSource);
    }
    Ok(reported)
}

/// The most bytes of a SessionStart's source that taw keeps.
const MOST_SOURCE: usize = 32;

/// Whether a report's source (startup, resume, clear, compact, select, new, branch) is one taw keeps,
/// 1 to [`MOST_SOURCE`] ASCII letters, digits, `_` or `-`: it comes from the less trusted side, and
/// goes into a tmux command and a token.
fn valid_source(source: &str) -> bool {
    (1..=MOST_SOURCE).contains(&source.len())
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// A string field that is there and not empty.
fn text<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// Python's truth of a JSON value, which the hooks test `agent_id` by.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_folder_is_named_for_the_server_by_its_socket_and_for_the_pane() {
        assert_eq!(
            key_of("/private/tmp/tmux-501/default"),
            "default-3d3d4a9387e30f49"
        );
        assert!(key_of("/tmp/x/taw-e2e-41-3").starts_with("taw-e2e-41-3-"));
        assert!(key_of("/tmp/x/my socket").starts_with("my_socket-"));
        assert_eq!(key_of(""), "default");
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        assert_eq!(
            folder(&paths, "default", "%31"),
            Some(paths.state_dir.join("sandbox/panes/default/%31"))
        );
    }

    /// Two servers whose sockets share a file name in two folders, the `default` of two
    /// `TMUX_TMPDIR`s, are two servers: their keys differ, and a pane of one never shares its report
    /// folder with the pane of the same id on the other. The same socket is the same key, each
    /// time, and a key is one plain name whatever the socket's name holds.
    #[test]
    fn two_sockets_of_one_name_in_two_folders_are_two_servers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let (one, other) = (
            key_of("/private/tmp/tmux-501/default"),
            key_of("/Users/you/tmux-tmp/tmux-501/default"),
        );
        assert_ne!(one, other);
        assert_ne!(folder(&paths, &one, "%0"), folder(&paths, &other, "%0"));
        assert_eq!(key_of("/private/tmp/tmux-501/default\n"), one);
        for socket in ["/tmp/a/..", "/tmp/a/.", "/tmp/x/a/b", "/", "/tmp/..."] {
            let key = key_of(socket);
            assert!(folder(&paths, &key, "%0").is_some(), "{socket:?}: {key:?}");
            assert!(!key.contains('/'), "{socket:?}: {key:?}");
        }
    }

    /// A server's name or a pane's that is not one plain name has no report folder: what it would
    /// make of a path of its own, or a climb out of taw's state, is never a folder taw empties.
    #[test]
    fn a_name_that_is_not_one_plain_name_has_no_report_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        for bad in ["", ".", "..", "../x", "/Users/you", "a/b", "x/", "a\0b"] {
            assert_eq!(folder(&paths, bad, "%31"), None, "server {bad:?}");
            assert_eq!(folder(&paths, "default", bad), None, "pane {bad:?}");
        }
        assert!(folder(&paths, ".hidden", "%0").is_some());
    }

    /// tmux's answer about the pane: its id, its terminal and its server's socket, whose path can hold
    /// a space; an answer that is not one says nothing.
    #[test]
    fn tmuxs_answer_about_the_pane_is_its_id_its_terminal_and_its_servers_name() {
        assert_eq!(
            answer_of("%31 /dev/ttys012 /private/tmp/x/my socket"),
            Some(Pane {
                id: "%31".to_string(),
                tty: PathBuf::from("/dev/ttys012"),
                server: key_of("/private/tmp/x/my socket")
            })
        );
        for odd in [
            "",
            "%31",
            "%31 /dev/ttys012",
            "%31 /dev/ttys012 ",
            "x31 /dev/ttys012 /tmp/s",
            "../x /dev/ttys012 /tmp/s",
            "%31 ttys012 /tmp/s",
            "  /tmp/s",
        ] {
            assert_eq!(answer_of(odd), None, "{odd:?}");
        }
    }

    /// The pane is asked of tmux by its id, and an answer for another pane, or for none, is a pane tmux
    /// does not know: stand-ins for tmux that answer right, for %7 when asked for %31, with an empty
    /// id (what tmux 3.6b prints for a pane it does not know), and one that fails.
    #[test]
    fn a_pane_tmux_knows_is_asked_of_it_and_an_answer_for_another_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let stand_in = |name: &str, body: &str| {
            let bin = dir.path().join(name);
            std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
            crate::test_exec::make_executable(&bin);
            Tmux { bin, socket: None }
        };

        let right = stand_in(
            "right",
            "[ \"$5\" = %31 ] && printf '%%31 /dev/ttys012 /private/tmp/tmux-501/default\\n'",
        );
        assert_eq!(
            ask(&right, "%31").unwrap().unwrap(),
            Pane {
                id: "%31".to_string(),
                tty: PathBuf::from("/dev/ttys012"),
                server: key_of("/private/tmp/tmux-501/default")
            }
        );
        let another = stand_in(
            "another",
            "printf '%%7 /dev/ttys012 /private/tmp/tmux-501/default\\n'",
        );
        assert_eq!(ask(&another, "%31").unwrap(), None);
        let empty = stand_in("empty", "printf '  /private/tmp/tmux-501/default\\n'");
        assert_eq!(ask(&empty, "%31").unwrap(), None);
        let none = stand_in("none", "echo \"no server running\" >&2; exit 1");
        assert!(format!("{:#}", ask(&none, "%31").unwrap_err()).contains("no server running"));
    }

    #[test]
    fn emptying_the_folder_makes_it_yours_alone_and_takes_links_away_as_links() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("panes/default/%31");
        std::fs::create_dir_all(folder.join("old")).unwrap();
        std::fs::write(folder.join(FILE), "{\"session_id\":\"last\"}").unwrap();
        let yours = dir.path().join("yours.txt");
        std::fs::write(&yours, "keep me").unwrap();
        std::os::unix::fs::symlink(&yours, folder.join("link")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("kept"), "keep me too").unwrap();
        std::os::unix::fs::symlink(&elsewhere, folder.join("folder-link")).unwrap();

        empty(&folder).unwrap();

        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
        assert_eq!(std::fs::read_to_string(&yours).unwrap(), "keep me");
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("kept")).unwrap(),
            "keep me too"
        );
        assert_eq!(
            std::fs::metadata(&folder).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// The folder taw empties is a folder of its own: where a link stands in its place, nothing is
    /// emptied and nothing is made, so that what the link leads to keeps everything it holds and
    /// the mode it had.
    #[test]
    fn a_link_where_the_folder_goes_is_refused_and_what_it_leads_to_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let yours = dir.path().join("documents");
        std::fs::create_dir_all(&yours).unwrap();
        std::fs::write(yours.join("notes.txt"), "keep me").unwrap();
        std::fs::set_permissions(&yours, std::fs::Permissions::from_mode(0o755)).unwrap();
        let folder = dir.path().join("panes/default/%31");
        std::fs::create_dir_all(folder.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&yours, &folder).unwrap();

        let err = empty(&folder).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "{} is a link, where taw keeps a folder of its own",
                folder.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(yours.join("notes.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            std::fs::metadata(&yours).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    use std::os::unix::ffi::OsStrExt;

    /// A temp folder whose report file `make` makes.
    fn folder_with(make: impl FnOnce(&Path)) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        make(&dir.path().join(FILE));
        dir
    }

    /// [`read`] of `folder` on a thread with a bound: a read that waits on a FIFO for a writer would
    /// hang the test run for ever, so after the bound it lets the reader go and fails the test.
    fn read_within_bound(folder: &Path) -> Result<Option<Found>, Ignored> {
        let (tx, rx) = std::sync::mpsc::channel();
        let there = folder.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(read(&there));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(read) => read,
            Err(_) => {
                use std::os::unix::fs::OpenOptionsExt;
                // A writer's open lets a reader blocked on the FIFO through.
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(folder.join(FILE));
                panic!("read waited on the report file");
            }
        }
    }

    #[test]
    fn a_regular_file_of_at_most_64_kib_is_read_and_known_again_until_it_changes() {
        let dir = folder_with(|file| std::fs::write(file, b"{}").unwrap());

        let found = read(dir.path()).unwrap().unwrap();
        assert_eq!(found.bytes, b"{}");
        assert_eq!(
            read(dir.path()).unwrap().unwrap().id,
            found.id,
            "the same file is known again"
        );

        std::fs::write(dir.path().join(FILE), vec![b' '; MOST as usize]).unwrap();
        let full = read(dir.path()).unwrap().unwrap();
        assert_eq!(full.bytes.len(), MOST as usize);
        assert_ne!(full.id, found.id, "a file that changed is another");
    }

    #[test]
    fn no_file_and_no_folder_are_no_report() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Ok(None));
        assert_eq!(read(&dir.path().join("gone")), Ok(None));
    }

    #[test]
    fn a_link_a_fifo_a_folder_and_a_file_over_64_kib_are_each_refused() {
        let yours = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            yours.path(),
            br#"{"hook_event_name":"SessionStart","session_id":"yours"}"#,
        )
        .unwrap();
        let link = folder_with(|file| std::os::unix::fs::symlink(yours.path(), file).unwrap());
        assert_eq!(read(link.path()), Err(Ignored::Link));

        let fifo = folder_with(|file| {
            let path = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        });
        // Without a writer: a read that waited for one would never come back.
        assert_eq!(read_within_bound(fifo.path()), Err(Ignored::NotAFile));

        let folder = folder_with(|file| std::fs::create_dir(file).unwrap());
        assert_eq!(read(folder.path()), Err(Ignored::NotAFile));

        let big = folder_with(|file| std::fs::write(file, vec![b' '; MOST as usize + 1]).unwrap());
        assert_eq!(read(big.path()), Err(Ignored::TooBig));
    }

    /// What the watcher's log says of a report it did not take is one of these, never what the file
    /// holds, and in a person's words.
    #[test]
    fn the_words_for_a_report_not_taken_hold_nothing_of_it_and_no_banned_mark() {
        use Ignored::*;
        for why in [
            Link,
            NotAFile,
            TooBig,
            Unreadable,
            NotJson,
            OtherAgent,
            SubAgent,
            NotSessionStart,
            NoSession,
            BadId,
            BadPath,
            BadSource,
            NoRule,
        ] {
            let said = why.to_string();
            assert!(
                said.starts_with("it ") || said.starts_with("its ") || said.starts_with("taw "),
                "{said}"
            );
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
                '{', '"',
            ] {
                assert!(!said.contains(banned), "{said}");
            }
        }
    }

    use serde_json::json;

    const CLAUDE: &str = r#"{"session_id":"4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d","transcript_path":"/home/sam/.claude/projects/-work-api/4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d.jsonl","cwd":"/work/api","hook_event_name":"SessionStart","source":"startup"}"#;
    const CODEX: &str = r#"{"session_id":"019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b","transcript_path":"/home/sam/.codex/sessions/2026/10/02/rollout-2026-10-02T09-12-01-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl","cwd":"/work/api","hook_event_name":"SessionStart","model":"gpt-5-codex","source":"startup"}"#;
    const OPENCODE: &str = r#"{"hook_event_name":"SessionStart","session_id":"ses_7f3a91c2bffe4Qz1Wm","source":"select","taw_agent":"opencode"}"#;

    /// `payload` with `key` set to `value`, or taken out with `None`.
    fn with(payload: &str, key: &str, value: Option<serde_json::Value>) -> Vec<u8> {
        let mut object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(payload).unwrap();
        match value {
            Some(value) => object.insert(key.to_string(), value),
            None => object.remove(key),
        };
        serde_json::to_vec(&object).unwrap()
    }

    #[test]
    fn a_session_start_gives_the_id_the_path_and_what_started_it() {
        assert_eq!(
            session_of("claude", CLAUDE.as_bytes()),
            Ok(Reported {
                id: Some("4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d".into()),
                path: Some(
                    "/home/sam/.claude/projects/-work-api/4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d.jsonl"
                        .into()
                ),
                source: Some("startup".into()),
            })
        );
        // Codex's hook keeps no path: its transcript path is evidence of a real start, no more.
        assert_eq!(
            session_of("codex", CODEX.as_bytes()),
            Ok(Reported {
                id: Some("019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b".into()),
                path: None,
                source: Some("startup".into()),
            })
        );
        assert_eq!(
            session_of("opencode", OPENCODE.as_bytes()),
            Ok(Reported {
                id: Some("ses_7f3a91c2bffe4Qz1Wm".into()),
                path: None,
                source: Some("select".into()),
            })
        );
    }

    #[test]
    fn a_sub_agents_report_another_event_and_another_agents_report_are_not_taken() {
        assert_eq!(
            session_of("claude", &with(CLAUDE, "agent_id", Some(json!("a1b2")))),
            Err(Ignored::SubAgent)
        );
        assert_eq!(
            session_of("codex", &with(CODEX, "agent_id", Some(json!("a1b2")))),
            Err(Ignored::SubAgent)
        );
        assert!(
            session_of("claude", &with(CLAUDE, "agent_id", Some(json!("")))).is_ok(),
            "an empty agent_id is the agent's own"
        );
        assert_eq!(
            session_of(
                "claude",
                &with(CLAUDE, "hook_event_name", Some(json!("SubagentStop")))
            ),
            Err(Ignored::NotSessionStart)
        );
        assert_eq!(
            session_of(
                "codex",
                &with(CODEX, "hook_event_name", Some(json!("Stop")))
            ),
            Err(Ignored::NotSessionStart)
        );
        assert_eq!(
            session_of(
                "claude",
                &with(CLAUDE, "cursor_version", Some(json!("1.2.3")))
            ),
            Err(Ignored::OtherAgent)
        );
        assert_eq!(
            session_of("opencode", &with(OPENCODE, MADE_BY, Some(json!("kilo")))),
            Err(Ignored::OtherAgent)
        );
        assert_eq!(
            session_of("claude", OPENCODE.as_bytes()),
            Err(Ignored::OtherAgent)
        );
        assert_eq!(
            session_of("gemini", CLAUDE.as_bytes()),
            Err(Ignored::NoRule)
        );
    }

    #[test]
    fn codex_needs_its_transcript_path_and_takes_a_report_with_no_event_name() {
        let bare = with(CODEX, "hook_event_name", None);
        assert_eq!(
            session_of("codex", &bare).map(|r| r.source),
            Ok(None),
            "no event, so no source, as its hook does"
        );
        // Its hook reads an empty event name as none, and any other value as an event that is not
        // SessionStart.
        let empty = with(CODEX, "hook_event_name", Some(json!("")));
        assert_eq!(session_of("codex", &empty).map(|r| r.source), Ok(None));
        for odd in [
            json!(7),
            json!(["SessionStart"]),
            json!({"a": 1}),
            json!(true),
        ] {
            assert_eq!(
                session_of("codex", &with(CODEX, "hook_event_name", Some(odd))),
                Err(Ignored::NotSessionStart)
            );
        }
        assert_eq!(
            session_of("codex", &with(CODEX, "transcript_path", Some(json!("  ")))),
            Err(Ignored::NotSessionStart)
        );
        assert_eq!(
            session_of("codex", &with(CODEX, "transcript_path", None)),
            Err(Ignored::NotSessionStart)
        );
    }

    #[test]
    fn what_is_not_one_object_or_names_no_session_taw_keeps_is_not_taken() {
        assert_eq!(
            session_of("claude", b"{\"session_id\":"),
            Err(Ignored::NotJson)
        );
        assert_eq!(session_of("claude", b"[1, 2]"), Err(Ignored::NotJson));
        assert_eq!(
            session_of(
                "claude",
                &with(CLAUDE, "session_id", Some(json!("4b1f; touch pwned")))
            ),
            Err(Ignored::BadId)
        );
        assert_eq!(
            session_of("claude", &with(CLAUDE, "session_id", Some(json!("")))),
            Err(Ignored::NoSession)
        );
        assert_eq!(
            session_of("claude", &with(CLAUDE, "session_id", Some(json!(42)))),
            Err(Ignored::NoSession)
        );
        assert_eq!(
            session_of(
                "claude",
                &with(
                    CLAUDE,
                    "transcript_path",
                    Some(json!("/home/sam/a\nb.jsonl"))
                )
            ),
            Err(Ignored::BadPath)
        );
        assert_eq!(
            session_of(
                "claude",
                &with(CLAUDE, "transcript_path", Some(json!("relative.jsonl")))
            ),
            Err(Ignored::BadPath)
        );
        assert_eq!(
            session_of("opencode", &with(OPENCODE, "session_id", None)),
            Err(Ignored::NoSession)
        );
    }

    /// A source is a short token (startup, resume, clear, compact, select, new, branch): another text
    /// refuses the report whole, as a bad id or path does, with nothing of it in the words.
    #[test]
    fn a_source_that_is_not_a_short_token_refuses_the_report_whole() {
        for source in [
            "startup", "resume", "clear", "compact", "select", "new", "branch", "a_b-C9",
        ] {
            for (agent, report) in [("claude", CLAUDE), ("codex", CODEX), ("opencode", OPENCODE)] {
                let taken = session_of(agent, &with(report, "source", Some(json!(source))));
                assert_eq!(
                    taken.map(|r| r.source),
                    Ok(Some(source.to_string())),
                    "{agent}: {source}"
                );
            }
        }
        let long = "a".repeat(33);
        let exact = "a".repeat(32);
        assert!(valid_source(&exact));
        assert!(!valid_source(&long));
        assert!(!valid_source(""));
        for source in [
            long.as_str(),
            "start up",
            "start\nup",
            "start\u{e9}",
            "a;b",
            "a.b",
        ] {
            for (agent, report) in [("claude", CLAUDE), ("codex", CODEX), ("opencode", OPENCODE)] {
                assert_eq!(
                    session_of(agent, &with(report, "source", Some(json!(source)))),
                    Err(Ignored::BadSource),
                    "{agent}: {source:?}"
                );
            }
        }
        // An empty source is none, as every hook reads it; one that is not text is none too.
        for none in [json!(""), json!(7), json!(null)] {
            assert_eq!(
                session_of("claude", &with(CLAUDE, "source", Some(none))).map(|r| r.source),
                Ok(None)
            );
        }
        // The probe: 20 KB of source inside a valid file.
        let big = "x".repeat(20_000);
        assert_eq!(
            session_of("claude", &with(CLAUDE, "source", Some(json!(big)))),
            Err(Ignored::BadSource)
        );
    }

    /// An integration's own report is a SessionStart: another event of the same agent is not taken.
    #[test]
    fn a_made_report_whose_event_is_not_a_session_start_is_not_taken() {
        for event in [json!("SessionEnd"), json!("Stop"), json!(7)] {
            assert_eq!(
                session_of("opencode", &with(OPENCODE, "hook_event_name", Some(event))),
                Err(Ignored::NotSessionStart)
            );
        }
        assert_eq!(
            session_of("opencode", &with(OPENCODE, "hook_event_name", None)),
            Err(Ignored::NotSessionStart)
        );
    }

    #[test]
    fn every_other_shell_hooks_report_is_read_under_its_own_rules() {
        let reported = |id: &str, path: Option<&str>, source: Option<&str>| {
            Ok(Reported {
                id: Some(id.to_string()),
                path: path.map(str::to_string),
                source: source.map(str::to_string),
            })
        };
        let cases: Vec<(&str, &str, Result<Reported, Ignored>)> = vec![
            (
                "copilot",
                r#"{"hook_event_name":"session_start","sessionId":"cp-1"}"#,
                reported("cp-1", None, None),
            ),
            (
                "copilot",
                r#"{"hook_event_name":"UserPromptSubmit","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"prompt":"hi","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"agent_id":"sub","session_id":"cp-1"}"#,
                Err(Ignored::SubAgent),
            ),
            (
                "copilot",
                r#"{"tool_name":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"toolName":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"notification_type":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"notificationType":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"stop_reason":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"stopReason":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"reason":"x","session_id":"cp-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "copilot",
                r#"{"agent_id":"","session_id":"cp-1"}"#,
                reported("cp-1", None, None),
            ),
            (
                "copilot",
                r#"{"agent_id":null,"session_id":"cp-1"}"#,
                reported("cp-1", None, None),
            ),
            (
                "copilot",
                r#"{"agent_id":false,"session_id":"cp-1"}"#,
                reported("cp-1", None, None),
            ),
            (
                "copilot",
                r#"{"agent_id":0,"session_id":"cp-1"}"#,
                reported("cp-1", None, None),
            ),
            (
                "cursor",
                r#"{"hook_event_name":"sessionStart","conversation_id":"cu-1"}"#,
                reported("cu-1", None, None),
            ),
            (
                "cursor",
                r#"{"conversationId":"cu-2"}"#,
                reported("cu-2", None, None),
            ),
            (
                "cursor",
                r#"{"hook_event_name":"stop","session_id":"cu-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "devin",
                r#"{"hook_event_name":"PreToolUse","sessionId":"dv-1"}"#,
                reported("dv-1", None, None),
            ),
            (
                "devin",
                r#"{"hook_event_name":"SessionStart","source":"startup"}"#,
                Err(Ignored::NoSession),
            ),
            (
                "droid",
                r#"{"session_id":"dr-1"}"#,
                reported("dr-1", None, None),
            ),
            (
                "qodercli",
                r#"{"session_id":"qd-1"}"#,
                reported("qd-1", None, None),
            ),
            (
                "grok",
                r#"{"hookEventName":"session_start","sessionId":"gk-1","source":"new"}"#,
                reported("gk-1", None, Some("new")),
            ),
            (
                "grok",
                r#"{"hook_event_name":"Stop","session_id":"gk-1"}"#,
                Err(Ignored::NotSessionStart),
            ),
            (
                "kimi",
                r#"{"session_id":"km-1"}"#,
                reported("km-1", None, Some("startup")),
            ),
            (
                "mastracode",
                r#"{"session_id":"mc-1"}"#,
                reported("mc-1", None, Some("startup")),
            ),
            (
                "agy",
                r#"{"conversationId":"ag-1","transcriptPath":"/home/sam/.gemini/tmp/ag-1.json"}"#,
                reported("ag-1", Some("/home/sam/.gemini/tmp/ag-1.json"), None),
            ),
            (
                "letta",
                r#"{"conversation_id":"default","agent_id":"agent-7","is_new_session":true}"#,
                reported("default:agent-7", None, Some("new")),
            ),
            (
                "letta",
                r#"{"conversation_id":"conv-9"}"#,
                reported("conv-9", None, Some("resume")),
            ),
            (
                "letta",
                r#"{"conversation_id":"conv-9","is_new_session":false}"#,
                reported("conv-9", None, Some("resume")),
            ),
            (
                "letta",
                r#"{"conversation_id":"conv-9","is_new_session":0}"#,
                reported("conv-9", None, Some("resume")),
            ),
            (
                "letta",
                r#"{"conversation_id":"conv-9","is_new_session":null}"#,
                reported("conv-9", None, Some("resume")),
            ),
            (
                "letta",
                r#"{"conversation_id":"conv-9","is_new_session":""}"#,
                reported("conv-9", None, Some("resume")),
            ),
            (
                "letta",
                r#"{"conversation_id":"conv-9","is_new_session":true}"#,
                reported("conv-9", None, Some("new")),
            ),
            (
                "letta",
                r#"{"conversation_id":"default"}"#,
                Err(Ignored::NoSession),
            ),
            (
                "qwen",
                r#"{"session_id":"qw-1","source":"branch"}"#,
                reported("qw-1", None, Some("branch")),
            ),
            (
                "qwen",
                r#"{"session_id":"qw-1","source":"made-up"}"#,
                reported("qw-1", None, None),
            ),
        ];
        for (agent, payload, expected) in cases {
            assert_eq!(
                session_of(agent, payload.as_bytes()),
                expected,
                "{agent} {payload}"
            );
        }
    }
}
