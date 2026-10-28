//! `taw image paste`: the clipboard's image (a stand-in clipboard tool on the PATH) into a stand-in agent that
//! turns bracketed paste on and records what arrives, on this machine and on another through the stand-in ssh.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::machine::{Failure, FarSide, StandIn};
use common::{Server, wait_for, write_script};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

/// A folder with stand-in clipboard tools (osascript for macOS, xclip for Linux) that give [`PNG`].
fn clipboard(dir: &Path) -> PathBuf {
    clipboard_of(dir, PNG)
}

/// A PNG over 1 MiB (the signature, any body): more than a pipe holds, so that it goes through in pieces.
fn big_png() -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend((0..(3 << 19)).map(|i: u32| (i % 251) as u8));
    assert!(png.len() > 1 << 20);
    png
}

/// A folder with stand-in clipboard tools that give `png`: the tools cat files holding it (the hex form of
/// osascript, the raw bytes of xclip), so that an image of any size is given as a real tool gives it.
fn clipboard_of(dir: &Path, png: &[u8]) -> PathBuf {
    let bin = dir.join("clip");
    std::fs::create_dir_all(&bin).unwrap();
    let hex: String = png.iter().map(|b| format!("{b:02X}")).collect();
    std::fs::write(
        bin.join("osascript.out"),
        format!("\u{ab}data PNGf{hex}\u{bb}\n"),
    )
    .unwrap();
    std::fs::write(bin.join("xclip.out"), png).unwrap();
    for tool in ["osascript", "xclip"] {
        write_script(
            &bin.join(tool),
            &format!(
                "#!/bin/sh\ncat '{}'\n",
                bin.join(format!("{tool}.out")).display()
            ),
        );
    }
    bin
}

/// Starts the stand-in agent in a new window of `server`: it turns bracketed paste on, says READY, and records
/// every byte typed at it into `record`. Gives its pane once READY is drawn (the mode is on by then).
fn agent(server: &Server, dir: &Path, record: &Path) -> String {
    let script = dir.join("agent.sh");
    write_script(
        &script,
        &format!(
            "#!/bin/sh\nstty raw -echo\nprintf '\\033[?2004hREADY'\nexec cat > '{}'\n",
            record.display()
        ),
    );
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        script.to_str().unwrap(),
    ]);
    let pane = pane.trim().to_string();
    assert!(
        wait_for(
            || server.capture(&pane).contains("READY"),
            Duration::from_secs(20)
        ),
        "the stand-in agent never drew READY: {:?}",
        server.capture(&pane)
    );
    pane
}

/// The bytes `record` holds once the paste's end marker is in it.
fn pasted(record: &Path) -> String {
    let mut seen = String::new();
    let came = wait_for(
        || {
            seen = String::from_utf8_lossy(&std::fs::read(record).unwrap_or_default()).into_owned();
            seen.contains("\x1b[201~")
        },
        Duration::from_secs(20),
    );
    assert!(came, "no bracketed paste arrived: {seen:?}");
    seen
}

fn path_with(server: &Server, first: &Path) -> String {
    let rest = server
        .taw_env()
        .into_iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value)
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    format!("{}:{rest}", first.display())
}

#[test]
fn the_image_is_written_here_and_its_path_pasted_between_the_markers() {
    let server = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("typed");
    let pane = agent(&server, dir.path(), &record);
    let path = path_with(&server, &clipboard(dir.path()));
    let (code, reply) =
        server.taw_with_env(&["image", "paste", "--pane", &pane], &[("PATH", &path)]);
    assert_eq!(code, 0, "{reply}");
    let file = reply["result"]["path"].as_str().unwrap().to_string();
    assert_eq!(reply["result"]["type"], "image_pasted");
    assert_eq!(pasted(&record), format!("\x1b[200~{file}\x1b[201~"));
    let images = server.state_dir().join("images");
    assert!(file.starts_with(images.to_str().unwrap()), "{file}");
    assert_eq!(std::fs::read(&file).unwrap(), PNG);
    let mode = std::fs::metadata(&images).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

/// A state folder whose path holds a space: the file is written there as it is named, and the path is pasted
/// with the space written `\ `, the form a terminal writes a dragged file in (Claude Code reads it back).
#[test]
fn a_space_in_the_state_path_is_pasted_escaped() {
    let server = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("typed");
    let pane = agent(&server, dir.path(), &record);
    let path = path_with(&server, &clipboard(dir.path()));
    let state = dir.path().join("state dir");
    let state_text = state.to_str().unwrap().to_string();
    let (code, reply) = server.taw_with_env(
        &["image", "paste", "--pane", &pane],
        &[("PATH", &path), ("TAW_STATE_DIR", &state_text)],
    );
    assert_eq!(code, 0, "{reply}");
    let file = reply["result"]["path"].as_str().unwrap().to_string();
    assert!(file.starts_with(&format!("{state_text}/images/")), "{file}");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        PNG,
        "the file is where its real path says"
    );
    assert_eq!(
        pasted(&record),
        format!("\x1b[200~{}\x1b[201~", file.replace(' ', "\\ "))
    );
}

#[test]
fn a_pane_that_is_gone_says_so() {
    let server = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let path = path_with(&server, &clipboard(dir.path()));
    let (code, reply) =
        server.taw_with_env(&["image", "paste", "--pane", "%999"], &[("PATH", &path)]);
    assert_ne!(code, 0);
    assert_eq!(reply["error"]["code"], "not_found", "{reply}");
    assert_eq!(reply["error"]["message"], "pane %999 is gone");
}

#[test]
fn the_image_goes_whole_to_a_machine_and_its_path_is_pasted_there() {
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("typed");
    let pane = agent(&studio.server, dir.path(), &record);
    let png = big_png();
    let path = path_with(&here, &clipboard_of(dir.path(), &png));
    let ssh = stand_in.bin.to_str().unwrap().to_string();
    let (code, reply) = here.taw_with_env(
        &["image", "paste", "--pane", &pane, "--machine", "studio"],
        &[("PATH", &path), ("TAW_SSH_BIN", &ssh)],
    );
    assert_eq!(code, 0, "{reply}");
    assert_eq!(reply["result"]["machine"], "studio");
    let file = reply["result"]["path"].as_str().unwrap().to_string();
    assert!(
        file.starts_with(studio.state_dir.join("images").to_str().unwrap()),
        "{file}"
    );
    assert!(
        std::fs::read(&file).unwrap() == png,
        "the bytes arrive whole ({} sent)",
        png.len()
    );
    assert_eq!(pasted(&record), format!("\x1b[200~{file}\x1b[201~"));
    assert!(
        !here.state_dir().join("images").exists(),
        "nothing is written here"
    );
}

#[test]
fn a_machine_whose_taw_is_too_old_says_so() {
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::OldTaw("0.1.0".to_string()));
    let dir = tempfile::tempdir().unwrap();
    let path = path_with(&here, &clipboard(dir.path()));
    let ssh = stand_in.bin.to_str().unwrap().to_string();
    let (code, reply) = here.taw_with_env(
        &["image", "paste", "--pane", "%1", "--machine", "studio"],
        &[("PATH", &path), ("TAW_SSH_BIN", &ssh)],
    );
    assert_ne!(code, 0);
    assert_eq!(reply["error"]["code"], "unsupported", "{reply}");
    assert_eq!(
        reply["error"]["message"],
        "studio's taw is too old to receive an image: update taw there"
    );
}

/// A far side that exits 0 and says nothing of where it wrote the image (a taw that does not know the verb, or no
/// taw at all behind the ssh) is told as a taw too old to receive an image, not as an image pasted.
#[test]
fn a_far_side_that_exits_0_without_an_answer_is_told_as_too_old() {
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let dir = tempfile::tempdir().unwrap();
    let path = path_with(&here, &clipboard(dir.path()));
    let calls = dir.path().join("calls");
    let silent = dir.path().join("silent-ssh");
    write_script(
        &silent,
        &format!(
            "#!/bin/sh\necho called >> '{}'\ncat > /dev/null\nexit 0\n",
            calls.display()
        ),
    );
    let ssh = silent.to_str().unwrap().to_string();
    let (code, reply) = here.taw_with_env(
        &["image", "paste", "--pane", "%1", "--machine", "studio"],
        &[("PATH", &path), ("TAW_SSH_BIN", &ssh)],
    );
    // The positive half: the ssh was run, and answered nothing.
    assert!(calls.exists(), "the stand-in ssh was never run");
    assert_ne!(code, 0, "{reply}");
    assert_eq!(reply["error"]["code"], "unsupported", "{reply}");
    assert_eq!(
        reply["error"]["message"],
        "studio's taw is too old to receive an image: update taw there"
    );
}

/// The environment a `taw image paste` that a screen starts needs to reach `here`'s tmux and folders, a stand-in
/// clipboard and the stand-in ssh, and the send bound `bound` seconds.
fn child_env(here: &Server, clip: &Path, ssh: Option<&Path>, bound: u32) -> Vec<(String, String)> {
    let mut env = here.taw_env();
    env.push(("PATH".to_string(), path_with(here, clip)));
    if let Some(ssh) = ssh {
        env.push(("TAW_SSH_BIN".to_string(), ssh.to_str().unwrap().to_string()));
    }
    env.push((taw::image::SEND_BOUND_VAR.to_string(), bound.to_string()));
    env
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A far side that is cut off after part of the bytes (a ssh that passes on the first 1000 and ends) writes
/// nothing in its images folder and pastes nothing: the sender says how many bytes it sends and the far side
/// takes the image only when exactly that many arrive.
#[test]
fn a_send_that_is_cut_writes_nothing_there_and_pastes_nothing() {
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("typed");
    let pane = agent(&studio.server, dir.path(), &record);
    let png = big_png();
    let path = path_with(&here, &clipboard_of(dir.path(), &png));
    // The cutting ssh: what it passes on to the stand-in is the first 1000 bytes of what it is given.
    let cutter = dir.path().join("cut-ssh");
    write_script(
        &cutter,
        &format!(
            "#!/bin/sh\nhead -c 1000 | exec '{}' \"$@\"\n",
            stand_in.bin.display()
        ),
    );
    let ssh = cutter.to_str().unwrap().to_string();
    let (code, reply) = here.taw_with_env(
        &["image", "paste", "--pane", &pane, "--machine", "studio"],
        &[("PATH", &path), ("TAW_SSH_BIN", &ssh)],
    );
    // The positive half: the far taw was reached and answered.
    assert!(
        !stand_in.calls().is_empty(),
        "the far side was never reached"
    );
    assert_ne!(code, 0, "{reply}");
    let message = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("studio: ") && message.contains("did not arrive whole"),
        "{reply}"
    );
    assert!(message.contains("bytes came"), "{reply}");
    let images = studio.state_dir.join("images");
    let written = std::fs::read_dir(&images)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(written, 0, "a part of the image was written");
    assert_eq!(
        std::fs::read(&record).unwrap_or_default(),
        b"",
        "something was pasted"
    );
}

/// A machine that cannot be reached (ssh exits 255) is named in the error.
#[test]
fn an_unreachable_machine_is_named_in_the_error() {
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::Refused);
    let dir = tempfile::tempdir().unwrap();
    let path = path_with(&here, &clipboard(dir.path()));
    let ssh = stand_in.bin.to_str().unwrap().to_string();
    let (code, reply) = here.taw_with_env(
        &["image", "paste", "--pane", "%1", "--machine", "studio"],
        &[("PATH", &path), ("TAW_SSH_BIN", &ssh)],
    );
    assert_ne!(code, 0);
    assert_eq!(reply["error"]["code"], "machine_unreachable", "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("studio: ") && message.contains("Connection refused"),
        "{reply}"
    );
}

/// A machine that connects and never answers is given up on at the send bound (shortened to 2 seconds for the
/// test): the error names the machine and the bound, and the ssh is ended.
#[test]
fn a_machine_that_never_answers_is_given_up_on_at_the_bound() {
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let path = path_with(&here, &clipboard(dir.path()));
    let ssh = stand_in.bin.to_str().unwrap().to_string();
    let began = std::time::Instant::now();
    let (code, reply) = here.taw_with_env(
        &["image", "paste", "--pane", "%1", "--machine", "studio"],
        &[
            ("PATH", &path),
            ("TAW_SSH_BIN", &ssh),
            (taw::image::SEND_BOUND_VAR, "2"),
        ],
    );
    assert_ne!(code, 0);
    assert_eq!(reply["error"]["code"], "timeout", "{reply}");
    assert_eq!(
        reply["error"]["message"],
        "no answer from studio within 2 seconds; the image may have arrived, so look before you paste again",
        "{reply}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(25),
        "the bound did not end it: {:?}",
        began.elapsed()
    );
    let pids = stand_in.pids();
    assert_eq!(pids.len(), 1, "the ssh was started once");
    assert!(
        wait_for(|| !alive(pids[0] as i32), Duration::from_secs(20)),
        "the ssh was left running"
    );
}

/// A screen that is left while a paste is under way does not wait for it: the paste is a process of its own in a
/// session of its own, which is still there, and ends by its own bound (4 seconds here). The positive half first:
/// the paste had reached the stand-in ssh, which hangs.
#[test]
fn leaving_with_a_paste_under_way_returns_at_once_and_the_paste_ends_by_its_bound() {
    use taw::tui::pasting::{Pasting, started_words};
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let env = child_env(&here, &clipboard(dir.path()), Some(&stand_in.bin), 4);
    let mut pasting = Pasting::spawning(common::taw_bin().into(), env);
    let tmux = taw::tmux::Tmux::with_socket_name(here.socket());
    let paths = taw::paths::Paths::rooted_at(
        here.config_dir().to_path_buf(),
        here.state_dir().to_path_buf(),
    );
    assert_eq!(
        pasting.start(&tmux, &paths, "studio:%1"),
        started_words("studio:%1")
    );
    assert!(
        wait_for(|| !stand_in.pids().is_empty(), Duration::from_secs(20)),
        "the paste never reached the ssh"
    );
    let ssh = stand_in.pids()[0] as i32;
    let child: i32 = {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &ssh.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    };
    assert!(alive(child));
    // SAFETY: getsid only reads the session of a process.
    let (own, theirs) = unsafe { (libc::getsid(0), libc::getsid(child)) };
    assert_eq!(theirs, child, "the paste leads a session of its own");
    assert_ne!(theirs, own);

    let began = std::time::Instant::now();
    drop(pasting);
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "leaving waited: {:?}",
        began.elapsed()
    );
    assert!(alive(child), "the paste ended with the screen");
    assert!(
        wait_for(|| !alive(child) && !alive(ssh), Duration::from_secs(30)),
        "the paste outlived its bound"
    );
}

/// While the screen lives, the toast is the paste process's own one-line answer.
#[test]
fn the_toast_comes_from_the_paste_processs_answer_while_the_screen_lives() {
    use taw::tui::pasting::{Pasting, UNDER_WAY, started_words};
    let here = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("typed");
    let pane = agent(&here, dir.path(), &record);
    let env = child_env(&here, &clipboard(dir.path()), None, 30);
    let mut pasting = Pasting::spawning(common::taw_bin().into(), env);
    let tmux = taw::tmux::Tmux::with_socket_name(here.socket());
    let paths = taw::paths::Paths::rooted_at(
        here.config_dir().to_path_buf(),
        here.state_dir().to_path_buf(),
    );
    assert_eq!(pasting.start(&tmux, &paths, &pane), started_words(&pane));
    let mut words = None;
    assert!(
        wait_for(
            || {
                words = pasting.poll();
                words.is_some()
            },
            Duration::from_secs(20)
        ),
        "no answer came"
    );
    assert_eq!(words.unwrap(), format!("image pasted into {pane}"));
    assert!(pasted(&record).contains(here.state_dir().join("images").to_str().unwrap()));
    // A second key right after the end starts a new paste and is not told one is under way.
    let mut said = Vec::new();
    pasting.press(&tmux, &paths, "%999", |words| said.push(words));
    assert!(!said.iter().any(|words| words == UNDER_WAY), "{said:?}");
    assert!(
        wait_for(
            || pasting
                .poll()
                .is_some_and(|words| words == "pane %999 is gone"),
            Duration::from_secs(20)
        ),
        "the second paste's answer never came"
    );
}

/// The paste process must not panic when nobody reads its answer any more (a popup that has closed): with
/// standard output a pipe whose reader is gone it ends with its own exit status, 1 for the timeout here, and
/// says nothing of a panic.
#[test]
fn the_paste_process_ignores_a_reader_that_has_gone() {
    use std::process::Stdio;
    let here = Server::start();
    here.write_config("[machines]\nstudio = \"studio\"\n");
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let path = path_with(&here, &clipboard(dir.path()));
    let ssh = stand_in.bin.to_str().unwrap().to_string();
    let mut cmd = here.taw_command(&["image", "paste", "--pane", "%1", "--machine", "studio"]);
    cmd.env("PATH", &path)
        .env("TAW_SSH_BIN", &ssh)
        .env(taw::image::SEND_BOUND_VAR, "2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    // The positive half: the paste is under way (its ssh is up) before the reader goes.
    assert!(
        wait_for(|| !stand_in.pids().is_empty(), Duration::from_secs(20)),
        "the paste never reached the ssh"
    );
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
}

/// A tmux stand-in that notes its arguments, one call per line, into `log` and runs the real tmux with them, and
/// the clipboard stand-in that waits for the file `gate` before it answers (so that a test can close the reader of
/// a paste's answer first). Gives the tmux to name in `TAW_TMUX_BIN` and the folder of clipboard tools.
fn noting_tmux(dir: &Path, log: &Path, gate: &Path) -> (PathBuf, PathBuf) {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let tmux = dir.join("noting-tmux");
    write_script(
        &tmux,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    );
    let clip = clipboard(dir);
    for tool in ["osascript", "xclip"] {
        write_script(
            &clip.join(tool),
            &format!(
                "#!/bin/sh\nwhile [ ! -f '{}' ]; do sleep 0.05; done\ncat '{}'\n",
                gate.display(),
                clip.join(format!("{tool}.out")).display()
            ),
        );
    }
    (tmux, clip)
}

/// Runs `taw image paste --pane <pane> --client <client>` with its answer's reader gone before it answers (the
/// screen has left), and gives the calls the noting tmux saw once it has ended.
fn paste_after_the_screen_left(pane: &str, here: &Server, dir: &Path) -> Vec<String> {
    use std::process::Stdio;
    let log = dir.join("tmux-calls");
    let gate = dir.join("gate");
    let (tmux, clip) = noting_tmux(dir, &log, &gate);
    let path = path_with(here, &clip);
    let mut cmd = here.taw_command(&["image", "paste", "--pane", pane, "--client", "client-7"]);
    cmd.env("PATH", &path)
        .env("TAW_TMUX_BIN", &tmux)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    // The screen leaves: its end of the answer's pipe goes. Then the clipboard answers.
    drop(child.stdout.take());
    std::fs::write(&gate, b"").unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A paste that fails after the screen has gone is shown on the client the screen was drawn on, with one
/// `display-message -c <client>`.
#[test]
fn a_failure_after_the_screen_has_gone_is_shown_on_its_client() {
    let here = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let calls = paste_after_the_screen_left("%999", &here, dir.path());
    // The positive half: the paste ran against the tmux.
    assert!(calls.iter().any(|call| call.contains("%999")), "{calls:?}");
    let shown: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains("display-message") && call.contains("-c client-7"))
        .collect();
    assert_eq!(shown.len(), 1, "{calls:?}");
    assert!(shown[0].ends_with("pane %999 is gone"), "{calls:?}");
}

/// A paste that succeeds after the screen has gone shows nothing on the client.
#[test]
fn a_success_after_the_screen_has_gone_shows_nothing() {
    let here = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("typed");
    let pane = agent(&here, dir.path(), &record);
    let calls = paste_after_the_screen_left(&pane, &here, dir.path());
    // The positive half: the image was pasted.
    assert!(pasted(&record).contains("images"), "nothing was pasted");
    assert!(
        calls.iter().any(|call| call.contains("paste-buffer")),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("-c client-7")),
        "{calls:?}"
    );
}

/// The machines page, COMPAT and SKILL say what the paste does: the refusal for an agent waiting on a question, leaving
/// at once while the paste finishes on its own, the length the far side is told, the names the cleanup takes and
/// the clipboard tool's 10 seconds.
#[test]
fn the_docs_say_what_the_paste_does() {
    let read = |file: &str| std::fs::read_to_string(common::repo_root().join(file)).unwrap();
    let (readme, compat, skill) = (
        read("docs/machines.md"),
        read("docs/COMPAT.md"),
        read("SKILL.md"),
    );
    // The verbs take any pane; only the list limits the paste to an agent's row.
    const ANY_PANE: &str = "paste into any pane they are given, a shell's too; only the list limits the paste to an agent's row";
    for (text, name, words) in [
        (
            &readme,
            "the machines page",
            "an agent that is waiting on a question (blocked, here or on a machine) says to answer it, then paste the image",
        ),
        (
            &readme,
            "the machines page",
            "leaving the screen is at once: the paste finishes on its own within its bounds (10 seconds for the clipboard tool, 30 for the send to a machine)",
        ),
        (
            &readme,
            "the machines page",
            "`taw image receive --bytes <n>` there, which writes and pastes only when exactly `n` bytes arrive",
        ),
        (
            &readme,
            "the machines page",
            "`image-<digits>.png` and `image-<digits>-<digits>.png`",
        ),
        (
            &compat,
            "COMPAT",
            "`taw image receive --pane <id> --bytes <n>`",
        ),
        (&compat, "COMPAT", "the image did not arrive whole"),
        (
            &compat,
            "COMPAT",
            "a clipboard tool that did not answer within 10 seconds (`timeout`)",
        ),
        (
            &compat,
            "COMPAT",
            "in a process of their own, in a session of their own",
        ),
        (
            &readme,
            "the machines page",
            "A paste that fails after you have left the popup is shown on the tmux client the popup was on",
        ),
        (
            &compat,
            "COMPAT",
            "is shown with `display-message -c` on the client the popup was on",
        ),
        (&skill, "SKILL", "`taw image receive --bytes <n>`"),
        (&readme, "the machines page", ANY_PANE),
        (&compat, "COMPAT", ANY_PANE),
        (&skill, "SKILL", ANY_PANE),
        (
            &skill,
            "SKILL",
            "The clipboard tool has 10 seconds to answer",
        ),
    ] {
        assert!(text.contains(words), "{name} no longer says {words:?}");
    }
}
