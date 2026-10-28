//! Pasting the image on this machine's clipboard into an agent's pane, on this machine or on a machine of
//! `[machines]`: `taw image paste` reads the clipboard and delivers the PNG; for another machine it streams the
//! bytes over that machine's ssh (the runner `taw --machine` uses) to `taw image receive` there, which
//! delivers them on its side. The sender says how many bytes it sends (`--bytes`), and the far side writes and
//! pastes nothing unless exactly that many arrived.
//!
//! Delivered means: written into the state folder's `images/` (readable by the user alone) under a timestamped
//! name, and the file's path pasted into the pane as a bracketed paste (`load-buffer`, then `paste-buffer -p`),
//! the way a terminal pastes a dragged file. Claude Code and Codex read an image file's path that arrives as a
//! paste as that image, attached to the prompt. Each delivery also removes the images older than [`KEPT_FOR`].

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cli::envelope::{CODES, CliError};
use crate::machine::ssh::{self, Mode, Ssh};
use crate::machine::{Machine, split_id, valid_tmux_id, words};
use crate::paths::Paths;
use crate::tmux::Tmux;

/// The folder under the state folder the images go in.
pub const IMAGES_DIR: &str = "images";

/// How long a delivered image is kept: a week. Each delivery removes the older ones.
pub const KEPT_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The most bytes an image may have, on either side.
pub const MOST_BYTES: usize = 64 * 1024 * 1024;

/// How long the delivery to another machine may take before its ssh is ended.
pub const SEND_BOUND: Duration = Duration::from_secs(30);

/// How long the clipboard tool may take to answer before it is ended.
pub const CLIPBOARD_BOUND: Duration = Duration::from_secs(10);

/// What the environment variable that shortens [`SEND_BOUND`] is called, in whole seconds: for a test that
/// would otherwise wait the thirty.
pub const SEND_BOUND_VAR: &str = "TAW_IMAGE_SEND_BOUND_SECS";

/// The eight bytes every PNG file starts with.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// What the paste says when the clipboard holds no image.
pub const NO_IMAGE: &str = "no image on the clipboard";

/// What the key says on a row that is not an agent's.
pub const NOT_AN_AGENT: &str = "only an agent row takes an image";

/// What the paste says when a Linux machine has clipboard tools but no display for them to reach (a machine
/// reached over ssh, say).
pub const NO_DISPLAY: &str =
    "no clipboard to read here (neither WAYLAND_DISPLAY nor DISPLAY is set)";

/// What the key says on a row whose agent is waiting on a question: the paste would go to the question's dialog,
/// which drops it.
pub const WAITING_ON_A_QUESTION: &str =
    "the agent is waiting on a question: answer it, then paste the image";

/// What a clipboard tool that did not answer in time says.
pub fn tool_timed_out(tool: &str, bound: Duration) -> String {
    format!(
        "the clipboard tool {tool} did not answer within {} seconds",
        bound.as_secs()
    )
}

/// What the far side says when fewer or more bytes arrived than the sender said it sent.
pub fn not_whole(expected: usize, got: usize, more: bool) -> String {
    let came = if more {
        format!("more than {expected} bytes came")
    } else {
        format!("{got} bytes came")
    };
    format!("the image did not arrive whole: {expected} bytes were to come and {came}")
}

/// What the paste says when this machine has none of the tools that read an image off the clipboard.
pub fn no_tool(platform: Platform) -> String {
    match platform {
        Platform::Mac => "no clipboard tool: osascript is not on the PATH".to_string(),
        Platform::Linux => {
            "no clipboard tool: install wl-clipboard (wl-paste) or xclip".to_string()
        }
    }
}

/// What a pane that is not there any more says.
pub fn pane_gone(pane: &str) -> String {
    format!("pane {pane} is gone")
}

/// What a machine whose taw has no `image receive` says.
pub fn too_old(machine: &str) -> String {
    format!("{machine}'s taw is too old to receive an image: update taw there")
}

/// What the list says once the image is in the pane: where it went.
pub fn pasted_words(pane: &str, machine: Option<&str>) -> String {
    match machine {
        Some(machine) => format!("image pasted into {pane} on {machine}"),
        None => format!("image pasted into {pane}"),
    }
}

/// Which clipboard tools are asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// `osascript`, asked for the clipboard as PNG data.
    Mac,
    /// `wl-paste --type image/png`, else `xclip -selection clipboard -t image/png -o`.
    Linux,
}

impl Platform {
    /// The platform taw runs on.
    pub fn this() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Linux
        }
    }
}

/// The image on this machine's clipboard, as PNG bytes, read by the tools found on `$PATH`.
pub fn read_clipboard() -> anyhow::Result<Vec<u8>> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let display = ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    read_clipboard_in(Platform::this(), &path, CLIPBOARD_BOUND, display)
}

/// [`read_clipboard`] for `platform`, with the tools looked for on `path` (a `PATH` value), each given `bound` to
/// answer, and `display` saying whether a display is reachable (`WAYLAND_DISPLAY` or `DISPLAY` is set: only a
/// Linux machine needs one).
pub fn read_clipboard_in(
    platform: Platform,
    path: &OsStr,
    bound: Duration,
    display: bool,
) -> anyhow::Result<Vec<u8>> {
    let tools: &[(&str, &[&str])] = match platform {
        Platform::Mac => &[(
            "osascript",
            &["-e", "the clipboard as \u{ab}class PNGf\u{bb}"],
        )],
        Platform::Linux => &[
            ("wl-paste", &["--type", "image/png"]),
            (
                "xclip",
                &["-selection", "clipboard", "-t", "image/png", "-o"],
            ),
        ],
    };
    let mut found = false;
    let mut timed_out = None;
    for (name, args) in tools {
        let Some(tool) = find_tool(path, name) else {
            continue;
        };
        found = true;
        let out = match run_within(&tool, args, bound) {
            Ran::Done(out) => out,
            Ran::TimedOut => {
                timed_out.get_or_insert(*name);
                continue;
            }
            Ran::NotRun => continue,
        };
        // An empty clipboard, or one holding no image, is a failure of the tool or nothing written.
        if !out.status.success() {
            continue;
        }
        let png = match platform {
            Platform::Mac => parse_osascript(&out.stdout),
            Platform::Linux => is_png(&out.stdout).then_some(out.stdout),
        };
        if let Some(png) = png {
            return Ok(png);
        }
    }
    if !found {
        Err(CliError::unsupported(no_tool(platform)))
    } else if let Some(tool) = timed_out {
        Err(CliError::timeout(tool_timed_out(tool, bound)))
    } else if platform == Platform::Linux && !display {
        Err(CliError::not_found(NO_DISPLAY))
    } else {
        Err(CliError::not_found(NO_IMAGE))
    }
}

/// What running a clipboard tool came to.
enum Ran {
    Done(std::process::Output),
    /// It had not ended when the bound passed: it was killed, with whatever it started, and reaped.
    TimedOut,
    NotRun,
}

/// Runs `tool` with `args`, standard input and error null, and gives its output, or ends it (its whole process
/// group, since a tool may be a script whose child holds the pipe) when it has not ended within `bound`.
fn run_within(tool: &Path, args: &[&str], bound: Duration) -> Ran {
    use std::os::unix::process::CommandExt;
    let Ok(mut child) = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return Ran::NotRun;
    };
    let stdout = read_on_thread(child.stdout.take());
    let deadline = Instant::now() + bound;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                end_group(&child);
                let _ = child.wait();
                return Ran::TimedOut;
            }
            Err(_) => return Ran::NotRun,
        }
    };
    // What the tool wrote is whole once it has ended; a helper that kept the pipe is not waited for long.
    // A helper that still holds the pipe after that is ended with the rest of the tool's group.
    let stdout = match stdout.recv_timeout(Duration::from_secs(2)) {
        Ok(bytes) => bytes,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            end_group(&child);
            Vec::new()
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Vec::new(),
    };
    Ran::Done(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// Ends the process group of `child`, which was started with `process_group(0)`.
fn end_group(child: &std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: signals the group of a child this run started as a group leader of its own. A group's id is
        // not given to another process while one of its members lives, and a group with none is not there.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

/// The PNG bytes of what `osascript` prints for the clipboard as PNG data, `\u{ab}data PNGf8950...\u{bb}`, or
/// nothing when it printed anything else (an empty clipboard prints nothing and fails).
pub fn parse_osascript(stdout: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    let hex = text
        .strip_prefix("\u{ab}data PNGf")?
        .strip_suffix('\u{bb}')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    is_png(&bytes).then_some(bytes)
}

/// Whether `bytes` start as a PNG file does.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(PNG_SIGNATURE)
}

/// The program `name` in the first folder of `path` that holds it as an executable file.
fn find_tool(path: &OsStr, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|file| {
            std::fs::metadata(file)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// `pane` when it is a pane id (`%` and digits), else the refusal.
fn pane_id(pane: &str) -> anyhow::Result<&str> {
    if pane.starts_with('%') && valid_tmux_id(pane) {
        Ok(pane)
    } else {
        Err(CliError::invalid_argument(format!(
            "{pane} is not a pane id (like %3)"
        )))
    }
}

/// What the paste did: the pane, the machine it is on (none for this one) and the image's path there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pasted {
    pub pane: String,
    pub machine: Option<String>,
    pub path: String,
}

/// `taw image paste`: the clipboard's image into `pane`, on `machine` when one is named.
pub fn paste(
    tmux: &Tmux,
    paths: &Paths,
    pane: &str,
    machine: Option<&str>,
) -> anyhow::Result<Pasted> {
    let pane = pane_id(pane)?;
    let Some(name) = machine else {
        let png = read_clipboard()?;
        let path = deliver_here(tmux, paths, pane, &png)?;
        return Ok(Pasted {
            pane: pane.to_string(),
            machine: None,
            path: path.display().to_string(),
        });
    };
    let machines = crate::machine::config::load_machines(paths)
        .map_err(|doubt| CliError::invalid_argument(doubt.to_string()))?;
    let Some(m) = machines.iter().find(|m| m.name == name) else {
        return Err(CliError::not_found(format!(
            "no machine called {name} in [machines]"
        )));
    };
    let png = read_clipboard()?;
    let path = send_to_machine(&Ssh::from_env(), paths, m, pane, &png)?;
    Ok(Pasted {
        pane: pane.to_string(),
        machine: Some(name.to_string()),
        path,
    })
}

/// The key on a row of the list: the clipboard's image into the pane `id` names (`%16`, or `studio:%16` on a
/// machine), and the words the toast says, whichever way it went.
pub fn paste_for_list(tmux: &Tmux, paths: &Paths, id: &str) -> String {
    let (machine, pane) = split_id(id);
    match paste(tmux, paths, pane, machine) {
        Ok(pasted) => pasted_words(&pasted.pane, pasted.machine.as_deref()),
        Err(err) => err.to_string(),
    }
}

/// What the toast says when the paste's own process ended with no answer to read.
pub const NO_ANSWER: &str = "the image paste ended without an answer";

/// The process that makes the paste for the list's pane id `id` (`%16`, or `studio:%16` on a machine): `taw
/// image paste --pane %16 [--machine studio]`, run by `taw_bin`. It runs in a session of its own (`setsid`), so
/// that a popup that closes, a signal to the screen or a hang-up of its terminal does not end it, with standard
/// input null and standard error null. Its answer, the one line of the verb's envelope, comes on a pipe the
/// screen reads for as long as it lives. It is pointed at the same tmux and folders as the screen is. `client`
/// is the tmux client the screen is drawn on, when it has one: a failure that comes after the screen has gone is
/// shown on it ([`show_on_client`]).
pub fn paste_command(
    taw_bin: &Path,
    tmux: &Tmux,
    paths: &Paths,
    id: &str,
    client: Option<&str>,
) -> Command {
    use crate::tmux::Socket;
    use std::os::unix::process::CommandExt;
    let (machine, pane) = split_id(id);
    let mut cmd = Command::new(taw_bin);
    cmd.args(["image", "paste", "--pane", pane]);
    if let Some(machine) = machine {
        cmd.args(["--machine", machine]);
    }
    if let Some(client) = client {
        cmd.args(["--client", client]);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("TAW_TMUX_BIN", &tmux.bin)
        .env("TAW_CONFIG_DIR", &paths.config_dir)
        .env("TAW_STATE_DIR", &paths.state_dir);
    match &tmux.socket {
        Some(Socket::Name(name)) => {
            cmd.env("TAW_TMUX_SOCKET_NAME", name);
        }
        Some(Socket::Path(path)) => {
            cmd.env_remove("TAW_TMUX_SOCKET_NAME")
                .env("TAW_TMUX_SOCKET_PATH", path);
        }
        None => {}
    }
    // SAFETY: the closure only calls `setsid`, which is async-signal-safe. It fails only for a process that
    // already leads a group, which a fresh child does not.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd
}

/// Runs the paste's process `cmd` ([`paste_command`]) to its end and gives the words the toast says: its answer
/// read as the list reads any paste's, or that it could not be started or ended with none.
pub fn run_paste_child(mut cmd: Command) -> String {
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return format!("could not start the image paste: {err}"),
    };
    let mut answer = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut answer);
    }
    let _ = child.wait();
    words_of_answer(&answer)
}

/// Shows `words` on the tmux client `client`, for a paste that failed after its screen had gone and cannot
/// toast it. Says nothing when the client is gone too.
pub fn show_on_client(tmux: &Tmux, client: &str, words: &str) {
    // `#` starts a format in a message: doubled, it is itself.
    let words = words.replace('#', "##");
    let _ = tmux.run_ok(&["display-message", "-c", client, "--", &words]);
}

/// The words of a `taw image paste` answer (its envelope on one line): where the image went, or why it did not.
pub fn words_of_answer(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let value = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    let Some(value) = value else {
        return NO_ANSWER.to_string();
    };
    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
    {
        return message.to_string();
    }
    let result = value.get("result");
    let field = |name: &str| {
        result
            .and_then(|result| result.get(name))
            .and_then(|field| field.as_str())
    };
    match field("pane") {
        Some(pane) => pasted_words(pane, field("machine")),
        None => NO_ANSWER.to_string(),
    }
}

/// The PNG on standard input, for `taw image receive`: exactly `expected` bytes, the length the sender said it
/// sends. Fewer (the send was cut) or more is an error, so that a part of an image is never written or pasted.
pub fn read_stdin(expected: usize) -> anyhow::Result<Vec<u8>> {
    read_whole(std::io::stdin().lock(), expected)
}

/// [`read_stdin`] over any reader.
pub fn read_whole(input: impl Read, expected: usize) -> anyhow::Result<Vec<u8>> {
    if expected > MOST_BYTES {
        return Err(CliError::invalid_argument(format!(
            "the image is larger than {} MiB",
            MOST_BYTES / (1024 * 1024)
        )));
    }
    let mut png = Vec::new();
    input.take(expected as u64 + 1).read_to_end(&mut png)?;
    if png.len() != expected {
        return Err(CliError::invalid_argument(not_whole(
            expected,
            png.len(),
            png.len() > expected,
        )));
    }
    Ok(png)
}

/// Delivers `png` to `pane` of this machine's tmux: written into the images folder, old images removed, and its
/// path pasted into the pane as a bracketed paste. The path written.
pub fn deliver_here(tmux: &Tmux, paths: &Paths, pane: &str, png: &[u8]) -> anyhow::Result<PathBuf> {
    let pane = pane_id(pane)?;
    if !is_png(png) {
        return Err(CliError::invalid_argument("what came is not a PNG image"));
    }
    match tmux.run(&["display-message", "-p", "-t", pane, "#{pane_id}"]) {
        Ok(found) if found == pane => {}
        _ => return Err(CliError::not_found(pane_gone(pane))),
    }
    let dir = images_dir(paths)?;
    let now = SystemTime::now();
    remove_old(&dir, now);
    let file = write_image(&dir, png, now)?;
    paste_path(tmux, pane, &file)?;
    Ok(file)
}

/// `<state>/images`, made readable by the user alone. A link in its place is refused.
pub fn images_dir(paths: &Paths) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = paths.state_dir.join(IMAGES_DIR);
    std::fs::create_dir_all(&paths.state_dir)?;
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }
    // A link is refused before anything is changed through it: its target is not taw's folder.
    if std::fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(CliError::invalid_argument(format!(
            "{} is a symbolic link: taw keeps its images in a folder of its own, so remove the link",
            dir.display()
        )));
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Writes `png` into `dir` under a name stamped with `now` (`image-<unix ms>.png`), never over a file there.
pub fn write_image(dir: &Path, png: &[u8], now: SystemTime) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for n in 0..100 {
        let name = if n == 0 {
            format!("image-{ms}.png")
        } else {
            format!("image-{ms}-{n}.png")
        };
        let file = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
        {
            Ok(mut out) => {
                out.write_all(png)?;
                return Ok(file);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(CliError::internal(
        "could not find a free name for the image",
    ))
}

/// Whether `name` is one [`write_image`] gives: `image-<digits>.png` or `image-<digits>-<digits>.png`. A file
/// of the user's in the folder (`image-old.png`) is not.
pub fn is_written_by_taw(name: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let Some(stem) = name
        .strip_prefix("image-")
        .and_then(|rest| rest.strip_suffix(".png"))
    else {
        return false;
    };
    match stem.split_once('-') {
        Some((ms, n)) => digits(ms) && digits(n),
        None => digits(stem),
    }
}

/// Removes the images taw wrote ([`is_written_by_taw`]) in `dir` last written more than [`KEPT_FOR`] before
/// `now`, and says how many went.
pub fn remove_old(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| is_written_by_taw(&entry.file_name().to_string_lossy()))
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age > KEPT_FOR)
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// The form of `file`'s path that is pasted: the path itself, with each space written `\ `, which is how a terminal
/// writes a dragged file whose path holds a space. Claude Code reads that form back (it removes the backslashes
/// before it opens the file), and a program that reads a pasted line as shell words (Codex) gets one word.
pub fn typed_form(file: &Path) -> Vec<u8> {
    let mut typed = Vec::new();
    for &byte in file.as_os_str().as_encoded_bytes() {
        if byte == b' ' {
            typed.push(b'\\');
        }
        typed.push(byte);
    }
    typed
}

/// Pastes `file`'s path ([`typed_form`]) into `pane` as a bracketed paste: the path is loaded into a buffer of its
/// own, which `paste-buffer -p` sends between the paste markers (when the program in the pane asked for them) and
/// `-d` deletes.
pub fn paste_path(tmux: &Tmux, pane: &str, file: &Path) -> anyhow::Result<()> {
    let buffer = format!("taw-image-{}", std::process::id());
    let mut cmd = tmux.command();
    cmd.args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&typed_form(file))?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(CliError::tmux(format!(
            "tmux load-buffer failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if let Err(err) = tmux.run(&["paste-buffer", "-p", "-d", "-b", &buffer, "-t", pane]) {
        // `-d` deletes the buffer only once it has been pasted: a failed paste leaves it for the server to keep.
        let _ = tmux.run(&["delete-buffer", "-b", &buffer]);
        return Err(CliError::not_found(format!("{}: {err}", pane_gone(pane))));
    }
    Ok(())
}

/// What a pipe of a child wrote, read on a thread of its own and sent when it ends.
fn read_on_thread(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (sent, got) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sent.send(bytes);
        });
    }
    got
}

/// The time a send to another machine may take: [`SEND_BOUND`], or what [`SEND_BOUND_VAR`] says in whole seconds
/// when that is from 1 to the thirty (a seam for tests that would otherwise wait the thirty: anything else, zero,
/// text, more than thirty or a number too large to add to a clock, means the thirty).
pub fn send_bound() -> Duration {
    bound_from(std::env::var(SEND_BOUND_VAR).ok().as_deref())
}

/// [`send_bound`] for the variable's value `text`, if it is set.
fn bound_from(text: Option<&str>) -> Duration {
    text.and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|secs| (1..=SEND_BOUND.as_secs()).contains(secs))
        .map_or(SEND_BOUND, Duration::from_secs)
}

/// What a send says when the far side had not answered by the bound: the image may be there all the same.
pub fn no_answer_in_time(machine: &str, bound: Duration) -> String {
    format!(
        "no answer from {machine} within {} seconds; the image may have arrived, so look before you paste again",
        bound.as_secs()
    )
}

/// Streams `png` to `taw image receive --pane <pane> --bytes <n>` on `m`, through the ssh runner, and gives the
/// path the far side wrote it to. The ssh is ended after [`send_bound`].
pub fn send_to_machine(
    ssh: &Ssh,
    paths: &Paths,
    m: &Machine,
    pane: &str,
    png: &[u8],
) -> anyhow::Result<String> {
    send_to_machine_within(ssh, paths, m, pane, png, send_bound())
}

/// [`send_to_machine`] with the ssh ended after `bound`. The far side is told how many bytes are coming and
/// takes nothing unless exactly that many arrive, so a send that is cut (the bound, a dropped connection) leaves
/// nothing written there and nothing pasted.
pub fn send_to_machine_within(
    ssh: &Ssh,
    paths: &Paths,
    m: &Machine,
    pane: &str,
    png: &[u8],
    bound: Duration,
) -> anyhow::Result<String> {
    let pane = pane_id(pane)?;
    let length = png.len().to_string();
    let name = &m.name;
    // Feed is the ssh that keeps its standard input open for what is written to it.
    let mut cmd = ssh::command(
        ssh,
        paths,
        m,
        Mode::Feed,
        &["image", "receive", "--pane", pane, "--bytes", &length],
    );
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| {
        CliError::machine_unreachable(format!("{name}: ssh could not be run ({err})"))
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = png.to_vec();
        // Dropped at the end of the write, which closes the far side's input.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let stdout = read_on_thread(child.stdout.take());
    let stderr = read_on_thread(child.stderr.take());
    let deadline = Instant::now() + bound;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CliError::timeout(no_answer_in_time(name, bound)));
            }
            Err(_) => break None,
        }
    };
    // A connection kept for the next command may hold a pipe open: what was written is not waited for long.
    let wait = Duration::from_secs(2);
    let stdout = stdout.recv_timeout(wait).unwrap_or_default();
    let stderr = stderr.recv_timeout(wait).unwrap_or_default();
    let unreachable = |why: String| CliError::machine_unreachable(format!("{name}: {why}"));
    match status.and_then(|status| status.code()) {
        // A far side that says nothing of where it wrote the image is one that does not know the verb.
        Some(0) => envelope_field(&stdout, "result", "path")
            .ok_or_else(|| CliError::unsupported(too_old(name))),
        Some(255) => {
            let line = ssh::last_line(&stderr);
            Err(unreachable(if line.is_empty() {
                "ssh exited 255 without saying why".to_string()
            } else {
                ssh::shown(&line)
            }))
        }
        Some(127) if String::from_utf8_lossy(&stderr).contains("not found") => {
            Err(unreachable(words::no_taw()))
        }
        Some(code) => match (
            envelope_field(&stdout, "error", "code"),
            envelope_field(&stdout, "error", "message"),
        ) {
            (Some(code), Some(message)) => {
                let code = CODES
                    .iter()
                    .find(|known| **known == code)
                    .copied()
                    .unwrap_or("internal");
                Err(CliError::with_code(
                    code,
                    format!("{name}: {}", ssh::shown(&message)),
                ))
            }
            // clap's complaint about a verb it does not know, status 2, and no envelope.
            _ if code == 2 => Err(CliError::unsupported(too_old(name))),
            _ => Err(unreachable(format!(
                "it ended with status {code} and no answer taw can read"
            ))),
        },
        None => Err(unreachable(
            "ssh was stopped before it answered".to_string(),
        )),
    }
}

/// `envelope[key][field]` of the last non-empty line of `stdout`, as a string.
fn envelope_field(stdout: &[u8], key: &str, field: &str) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(value.get(key)?.get(field)?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// A folder holding stand-in tools, each a shell script.
    fn tools(scripts: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in scripts {
            let file = dir.path().join(name);
            std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
            crate::test_exec::make_executable(&file);
        }
        dir
    }

    #[test]
    fn osascript_png_data_reads_back_as_the_png_bytes() {
        let printed = format!("\u{ab}data PNGf{}\u{bb}\n", hex(PNG));
        assert_eq!(parse_osascript(printed.as_bytes()), Some(PNG.to_vec()));
    }

    #[test]
    fn anything_else_osascript_prints_is_no_image() {
        for printed in [
            String::new(),
            "\n".to_string(),
            "\u{ab}data PNGf\u{bb}".to_string(),
            format!("\u{ab}data TIFF{}\u{bb}", hex(PNG)),
            format!("\u{ab}data PNGf{}0\u{bb}", hex(PNG)),
            "\u{ab}data PNGf00112233\u{bb}".to_string(),
            "some text".to_string(),
        ] {
            assert_eq!(parse_osascript(printed.as_bytes()), None, "{printed:?}");
        }
    }

    #[test]
    fn the_stand_in_osascript_on_the_path_gives_the_image() {
        let dir = tools(&[(
            "osascript",
            &format!("printf '%s\\n' '\u{ab}data PNGf{}\u{bb}'", hex(PNG)),
        )]);
        let png = read_clipboard_in(Platform::Mac, dir.path().as_os_str(), CLIPBOARD_BOUND, true)
            .unwrap();
        assert_eq!(png, PNG);
    }

    #[test]
    fn an_empty_clipboard_says_there_is_no_image() {
        // What osascript does when the clipboard holds no PNG: an error on stderr and status 1.
        let dir = tools(&[(
            "osascript",
            "echo \"execution error: Can't make some data into the expected type. (-1700)\" >&2; exit 1",
        )]);
        let err = read_clipboard_in(Platform::Mac, dir.path().as_os_str(), CLIPBOARD_BOUND, true)
            .unwrap_err();
        assert_eq!(err.to_string(), NO_IMAGE);
        assert_eq!(crate::cli::envelope::code_for(&err), "not_found");
    }

    #[test]
    fn no_clipboard_tool_on_the_path_says_so() {
        let dir = tools(&[]);
        for platform in [Platform::Mac, Platform::Linux] {
            let err = read_clipboard_in(platform, dir.path().as_os_str(), CLIPBOARD_BOUND, true)
                .unwrap_err();
            assert_eq!(err.to_string(), no_tool(platform));
            assert_eq!(crate::cli::envelope::code_for(&err), "unsupported");
        }
    }

    #[test]
    fn linux_asks_xclip_when_wl_paste_gives_no_image() {
        let dir = tools(&[
            ("wl-paste", "exit 1"),
            (
                "xclip",
                "printf '\\211PNG\\r\\n\\032\\n\\000\\000\\000\\rIHDR'",
            ),
        ]);
        let png = read_clipboard_in(
            Platform::Linux,
            dir.path().as_os_str(),
            CLIPBOARD_BOUND,
            true,
        )
        .unwrap();
        assert_eq!(png, PNG);
        let empty = tools(&[("xclip", "exit 0")]);
        let err = read_clipboard_in(
            Platform::Linux,
            empty.path().as_os_str(),
            CLIPBOARD_BOUND,
            true,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), NO_IMAGE);
    }

    #[test]
    fn images_older_than_seven_days_are_removed_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = write_image(dir.path(), PNG, now).unwrap();
        let young = dir.path().join("image-2.png");
        std::fs::write(&young, PNG).unwrap();
        let other = dir.path().join("notes.txt");
        std::fs::write(&other, "x").unwrap();
        let eight_days = now - Duration::from_secs(8 * 24 * 60 * 60);
        for file in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(eight_days)
                .unwrap();
        }
        let six_days = now - Duration::from_secs(6 * 24 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&young)
            .unwrap()
            .set_modified(six_days)
            .unwrap();
        assert_eq!(remove_old(dir.path(), now), 1);
        assert!(!old.exists());
        assert!(young.exists());
        assert!(other.exists(), "only taw's own images are removed");
    }

    #[test]
    fn the_images_folder_is_the_users_alone_and_names_never_collide() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(root.path().join("c"), root.path().join("s"));
        let dir = images_dir(&paths).unwrap();
        assert_eq!(dir, root.path().join("s").join(IMAGES_DIR));
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let now = SystemTime::now();
        let first = write_image(&dir, PNG, now).unwrap();
        let second = write_image(&dir, PNG, now).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&second).unwrap(), PNG);
    }

    /// A path with no space is pasted as it is; each space of one that has any is written `\ `, the form a
    /// terminal writes a dragged file in, which Claude Code reads back (it removes the backslashes).
    #[test]
    fn a_space_in_the_pasted_path_is_written_escaped() {
        assert_eq!(
            typed_form(Path::new("/home/me/.local/state/taw/images/image-1.png")),
            b"/home/me/.local/state/taw/images/image-1.png"
        );
        assert_eq!(
            typed_form(Path::new("/Users/Jo Bloggs/state dir/images/image-1.png")),
            b"/Users/Jo\\ Bloggs/state\\ dir/images/image-1.png"
        );
    }

    #[test]
    fn only_a_pane_id_is_taken() {
        assert!(pane_id("%3").is_ok());
        for bad in ["3", "$3", "@3", "%", "%3;x", "studio:%3"] {
            assert!(pane_id(bad).is_err(), "{bad}");
        }
    }

    /// A Linux machine whose clipboard tools all fail with no display to reach (a machine reached over ssh) says
    /// so, in place of "no image on the clipboard"; with a display, or on a Mac, an empty clipboard is still that.
    #[test]
    fn a_linux_machine_with_no_display_says_there_is_no_clipboard_to_read() {
        let dir = tools(&[("wl-paste", "exit 1"), ("xclip", "exit 1")]);
        let path = dir.path().as_os_str();
        let err = read_clipboard_in(Platform::Linux, path, CLIPBOARD_BOUND, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no clipboard to read here (neither WAYLAND_DISPLAY nor DISPLAY is set)"
        );
        assert_eq!(crate::cli::envelope::code_for(&err), "not_found");
        let err = read_clipboard_in(Platform::Linux, path, CLIPBOARD_BOUND, true).unwrap_err();
        assert_eq!(
            err.to_string(),
            NO_IMAGE,
            "with a display it is an empty clipboard"
        );
        let mac = tools(&[("osascript", "exit 1")]);
        let err = read_clipboard_in(
            Platform::Mac,
            mac.path().as_os_str(),
            CLIPBOARD_BOUND,
            false,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), NO_IMAGE, "a Mac needs no display variable");
    }

    /// Whether a process is still there (a zombie is there until it is waited for).
    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// A clipboard tool that does not answer is ended, with what it started, when the bound passes, and the error
    /// says the tool did not answer within that time.
    #[test]
    fn a_clipboard_tool_that_never_answers_is_ended_at_its_bound() {
        let marks = tempfile::tempdir().unwrap();
        let (own, helper) = (marks.path().join("own"), marks.path().join("helper"));
        let dir = tools(&[(
            "osascript",
            &format!(
                "echo $$ > '{}'; sleep 30 & echo $! > '{}'; wait",
                own.display(),
                helper.display()
            ),
        )]);
        let began = Instant::now();
        let err = read_clipboard_in(
            Platform::Mac,
            dir.path().as_os_str(),
            Duration::from_secs(1),
            true,
        )
        .unwrap_err();
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "the tool was waited for: {:?}",
            began.elapsed()
        );
        assert_eq!(
            err.to_string(),
            tool_timed_out("osascript", Duration::from_secs(1))
        );
        assert_eq!(
            tool_timed_out("osascript", CLIPBOARD_BOUND),
            "the clipboard tool osascript did not answer within 10 seconds"
        );
        assert_eq!(crate::cli::envelope::code_for(&err), "timeout");
        let pids: Vec<i32> = [&own, &helper]
            .iter()
            .map(|file| {
                std::fs::read_to_string(file)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap()
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(20);
        while pids.iter().any(|pid| alive(*pid)) {
            assert!(
                Instant::now() < deadline,
                "the tool or its helper is still running: {pids:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A tool that ends while a helper it started still holds its output is not left with that helper: the
    /// helper is ended too once the read of the output has waited its two seconds.
    #[test]
    fn a_helper_that_holds_the_tools_output_after_it_ended_is_ended_too() {
        let marks = tempfile::tempdir().unwrap();
        let helper = marks.path().join("helper");
        let dir = tools(&[(
            "osascript",
            &format!("sleep 30 & echo $! > '{}'; exit 1", helper.display()),
        )]);
        let began = Instant::now();
        let err = read_clipboard_in(Platform::Mac, dir.path().as_os_str(), CLIPBOARD_BOUND, true)
            .unwrap_err();
        assert_eq!(err.to_string(), NO_IMAGE);
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "the helper was waited for: {:?}",
            began.elapsed()
        );
        let pid: i32 = std::fs::read_to_string(&helper)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while alive(pid) {
            assert!(
                Instant::now() < deadline,
                "the helper is still running: {pid}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The seam that shortens the send's bound honours only 1 to 30 seconds: zero, text, 31 and a number too large
    /// to add to a clock all mean the thirty (and the bound can be added to an `Instant`).
    #[test]
    fn the_send_bound_seam_takes_only_one_to_thirty_seconds() {
        assert_eq!(bound_from(None), SEND_BOUND);
        for (text, secs) in [("1", 1), ("2", 2), (" 30 ", 30), ("30", 30)] {
            assert_eq!(
                bound_from(Some(text)),
                Duration::from_secs(secs),
                "{text:?}"
            );
        }
        for text in ["0", "junk", "", "-1", "31", "18446744073709551615"] {
            assert_eq!(bound_from(Some(text)), SEND_BOUND, "{text:?}");
            assert!(Instant::now().checked_add(bound_from(Some(text))).is_some());
        }
    }

    /// The text of a send that was not answered in time says the image may have arrived.
    #[test]
    fn a_send_that_timed_out_says_the_image_may_have_arrived() {
        assert_eq!(
            no_answer_in_time("studio", SEND_BOUND),
            "no answer from studio within 30 seconds; the image may have arrived, so look before you paste again"
        );
    }

    /// Only the names taw writes are the cleanup's: a file of the user's called `image-old.png` stays, however old.
    #[test]
    fn the_cleanup_removes_only_the_names_taw_writes() {
        for name in ["image-1.png", "image-1700000000000.png", "image-1-2.png"] {
            assert!(is_written_by_taw(name), "{name}");
        }
        for name in [
            "image-old.png",
            "image-.png",
            "image-1-.png",
            "image--2.png",
            "image-1-x.png",
            "image-1-2-3.png",
            "image-1.png.bak",
            "image-1.PNG",
            "xyz-1.png",
            "image-1",
        ] {
            assert!(!is_written_by_taw(name), "{name}");
        }
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let eight_days = now - Duration::from_secs(8 * 24 * 60 * 60);
        let (mine, users) = (
            ["image-1.png", "image-2-1.png"],
            ["image-old.png", "image-1.png.bak", "notes.png"],
        );
        for name in mine.iter().chain(users.iter()) {
            let file = dir.path().join(name);
            std::fs::write(&file, PNG).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(eight_days)
                .unwrap();
        }
        assert_eq!(remove_old(dir.path(), now), 2);
        for name in mine {
            assert!(!dir.path().join(name).exists(), "{name} went");
        }
        for name in users {
            assert!(dir.path().join(name).exists(), "{name} stays");
        }
    }

    /// An `images` that is a link is refused: no permission is changed through it, and nothing is written there.
    #[test]
    fn an_images_folder_that_is_a_link_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(root.path().join("c"), root.path().join("s"));
        let target = root.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::os::unix::fs::symlink(&target, paths.state_dir.join(IMAGES_DIR)).unwrap();
        let err = images_dir(&paths).unwrap_err();
        assert!(err.to_string().contains("is a symbolic link"), "{err}");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the link's target was not touched");
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        // The positive half: a real folder in its place is taken.
        std::fs::remove_file(paths.state_dir.join(IMAGES_DIR)).unwrap();
        assert!(images_dir(&paths).is_ok());
    }

    /// The far side takes exactly the bytes the sender said it sends: fewer (a cut send) or more is refused with
    /// words that say the image did not arrive whole.
    #[test]
    fn the_far_side_takes_exactly_the_announced_bytes() {
        use std::io::Cursor;
        let whole = [PNG, &[7u8; 100][..]].concat();
        assert_eq!(
            read_whole(Cursor::new(whole.clone()), whole.len()).unwrap(),
            whole
        );
        let cut = read_whole(Cursor::new(whole[..50].to_vec()), whole.len()).unwrap_err();
        assert_eq!(
            cut.to_string(),
            format!(
                "the image did not arrive whole: {} bytes were to come and 50 bytes came",
                whole.len()
            )
        );
        let more = read_whole(Cursor::new(whole.clone()), whole.len() - 1).unwrap_err();
        assert!(
            more.to_string().contains("the image did not arrive whole"),
            "{more}"
        );
        let huge = read_whole(Cursor::new(Vec::new()), MOST_BYTES + 1).unwrap_err();
        assert!(huge.to_string().contains("larger than"), "{huge}");
    }

    /// A tmux that fails the paste leaves no buffer of ours behind: after `load-buffer` and a failed
    /// `paste-buffer`, the `taw-image-<pid>` buffer is deleted.
    #[test]
    fn a_failed_paste_deletes_the_buffer_it_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n[ \"$1\" = -u ] && shift\nprintf '%s\\n' \"$*\" >> '{}'\n\
                 case \"$1\" in\n  load-buffer) cat > /dev/null ;;\n  paste-buffer) echo \"can't find pane\" >&2; exit 1 ;;\nesac\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        {
            crate::test_exec::make_executable(&bin);
        }
        let tmux = Tmux { bin, socket: None };
        let err = paste_path(&tmux, "%7", Path::new("/x/images/image-1.png")).unwrap_err();
        assert!(err.to_string().contains("pane %7 is gone"), "{err}");
        let calls = std::fs::read_to_string(&log).unwrap();
        let buffer = format!("taw-image-{}", std::process::id());
        assert!(
            calls.contains(&format!("load-buffer -b {buffer} -")),
            "{calls}"
        );
        assert!(
            calls.contains(&format!("delete-buffer -b {buffer}")),
            "the buffer was left in tmux: {calls}"
        );
    }

    #[test]
    fn the_paste_process_is_taw_image_paste_for_the_pane_and_machine_of_the_row() {
        let paths = Paths::rooted_at("/c".into(), "/s".into());
        let tmux = Tmux {
            bin: "/t/tmux".into(),
            socket: Some(crate::tmux::Socket::Path("/p.sock".into())),
        };
        let args = |cmd: &Command| -> Vec<String> {
            cmd.get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let here = paste_command(Path::new("/b/taw"), &tmux, &paths, "%16", None);
        assert_eq!(here.get_program(), "/b/taw");
        assert_eq!(args(&here), ["image", "paste", "--pane", "%16"]);
        let there = paste_command(Path::new("/b/taw"), &tmux, &paths, "studio:%16", None);
        assert_eq!(
            args(&there),
            ["image", "paste", "--pane", "%16", "--machine", "studio"]
        );
        let shown = paste_command(
            Path::new("/b/taw"),
            &tmux,
            &paths,
            "%16",
            Some("/dev/ttys7"),
        );
        assert_eq!(
            args(&shown),
            ["image", "paste", "--pane", "%16", "--client", "/dev/ttys7"]
        );
        let env = |cmd: &Command, name: &str| {
            cmd.get_envs()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(
            env(&here, "TAW_TMUX_SOCKET_PATH"),
            Some(Some("/p.sock".to_string()))
        );
        assert_eq!(env(&here, "TAW_TMUX_SOCKET_NAME"), Some(None), "taken out");
        assert_eq!(env(&here, "TAW_STATE_DIR"), Some(Some("/s".to_string())));
        assert_eq!(
            env(&here, "TAW_TMUX_BIN"),
            Some(Some("/t/tmux".to_string()))
        );
    }

    #[test]
    fn the_paste_process_answer_is_read_as_the_toast() {
        let ok = br#"{"ok":true,"result":{"type":"image_pasted","pane":"%16","machine":"studio","path":"/x"}}"#;
        assert_eq!(words_of_answer(ok), "image pasted into %16 on studio");
        let here = br#"{"result":{"pane":"%16","machine":null,"path":"/x"}}"#;
        assert_eq!(words_of_answer(here), "image pasted into %16");
        let err =
            br#"{"error":{"code":"timeout","message":"no answer from studio within 30 seconds"}}"#;
        assert_eq!(
            words_of_answer(err),
            "no answer from studio within 30 seconds"
        );
        assert_eq!(words_of_answer(b""), NO_ANSWER);
        assert_eq!(words_of_answer(b"not json"), NO_ANSWER);
    }
}
