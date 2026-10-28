//! A stand-in for a terminal that knows whether it is light or dark: a pseudo terminal whose slave side runs a
//! tmux client (`attach`) of a [`Server`], with `TERM=xterm-256color`, and whose master side this test reads and
//! writes. tmux 3.8's client asks its terminal `ESC [ ? 996 n` (and turns on reports with `ESC [ ? 2031 h`); a
//! terminal that knows its mode answers `ESC [ ? 997 ; 1 n` (dark) or `ESC [ ? 997 ; 2 n` (light), and with 2031 on
//! sends the same unasked when the mode changes. This one answers what the test set, and pushes the other mode on
//! request. A client attached inside another tmux never gets such an answer, which is why this exists.
//!
//! It owns the pids it starts: the client is ended by pid in `Drop`, and the reader thread ends with the master.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::Server;

/// What the terminal answers when asked: nothing, dark or light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Unreported,
    Dark,
    Light,
}

impl Mode {
    fn code(self) -> u8 {
        match self {
            Mode::Unreported => 0,
            Mode::Dark => 1,
            Mode::Light => 2,
        }
    }

    fn from_code(code: u8) -> Mode {
        match code {
            1 => Mode::Dark,
            2 => Mode::Light,
            _ => Mode::Unreported,
        }
    }
}

/// The report of `mode`: `ESC [ ? 997 ; 1 n` for dark, `ESC [ ? 997 ; 2 n` for light.
fn report(mode: Mode) -> Option<&'static [u8]> {
    match mode {
        Mode::Dark => Some(b"\x1b[?997;1n"),
        Mode::Light => Some(b"\x1b[?997;2n"),
        Mode::Unreported => None,
    }
}

/// The questions a terminal is asked that this one answers as a modern xterm does, besides the light-or-dark one.
const DEVICE_QUESTIONS: &[(&[u8], &[u8])] = &[
    // Primary device attributes: a VT220 with colour.
    (b"\x1b[c", b"\x1b[?62;22c"),
    (b"\x1b[0c", b"\x1b[?62;22c"),
    // Secondary device attributes: xterm 384.
    (b"\x1b[>c", b"\x1b[>41;384;0c"),
    (b"\x1b[>0c", b"\x1b[>41;384;0c"),
];

const ASK_MODE: &[u8] = b"\x1b[?996n";
const REPORTS_ON: &[u8] = b"\x1b[?2031h";

/// A tmux client on a pseudo terminal that this test answers for.
pub struct Terminal {
    /// The client's pid, ended in `Drop`.
    child: Child,
    master: Arc<Mutex<std::fs::File>>,
    /// Everything the client wrote to its terminal, in order.
    written: Arc<Mutex<Vec<u8>>>,
    mode: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// The client's name as the server knows it (the tty path), for `list-clients` and `-c`.
    pub tty: String,
}

impl Terminal {
    /// Attaches a client to `session` of `server` on a new pseudo terminal that answers `mode` to the question of
    /// its light or dark mode, and waits (20 s) until the server lists it.
    pub fn attach(server: &Server, session: &str, mode: Mode) -> Terminal {
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `openpty` writes the two descriptors; the name and termios are its defaults.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size as *const libc::winsize as *mut libc::winsize,
            )
        };
        assert_eq!(opened, 0, "no pseudo terminal");
        // Kept from the processes other tests start meanwhile: a stand-in that a parallel test runs would hold the
        // master open until it ended. They are set after `openpty` returns, so a test that forks in that window
        // inherits both descriptors; that child is harmless: `Drop` kills the client by its pid and the reader
        // ends on its stop flag, not on end of file, so a late copy of the master holds nothing up.
        for fd in [master, slave] {
            // SAFETY: `fcntl` of descriptors made here.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // SAFETY: both are open descriptors that only this function owns.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: `ttyname` of the slave just made, copied at once.
        let tty = unsafe {
            std::ffi::CStr::from_ptr(libc::ttyname(slave.as_raw_fd()))
                .to_string_lossy()
                .into_owned()
        };

        let mut cmd = Command::new("tmux");
        cmd.arg("-L")
            .arg(&server.socket)
            .args(["attach", "-t", session]);
        for name in taw::git::REDIRECTING_VARIABLES {
            cmd.env_remove(name);
        }
        server.apply_base_env(&mut cmd);
        cmd.env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: between fork and exec only async-signal-safe calls: a new session, and the slave as its terminal.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("tmux attach did not start");
        drop(slave);

        let master = std::fs::File::from(master);
        let reader_side = master.try_clone().unwrap();
        let master = Arc::new(Mutex::new(master));
        let written = Arc::new(Mutex::new(Vec::new()));
        let mode = Arc::new(AtomicU8::new(mode.code()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (master, written, mode, stop) =
                (master.clone(), written.clone(), mode.clone(), stop.clone());
            std::thread::spawn(move || read_loop(reader_side, &master, &written, &mode, &stop))
        };
        let terminal = Terminal {
            child,
            master,
            written,
            mode,
            stop,
            reader: Some(reader),
            tty,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !server
            .tmux(&["list-clients", "-F", "#{client_name}"])
            .unwrap_or_default()
            .lines()
            .any(|name| name == terminal.tty)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the server never listed the client on {}; it wrote {}",
                terminal.tty,
                terminal.head_hex(256)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        terminal
    }

    /// Sets the mode that the next question is answered with, and pushes it unasked as a terminal does when its
    /// mode changes (a report tmux has turned on with `ESC [ ? 2031 h`).
    pub fn push(&self, mode: Mode) {
        self.mode.store(mode.code(), Ordering::SeqCst);
        if let Some(bytes) = report(mode) {
            let mut master = self.master.lock().unwrap();
            let _ = master.write_all(bytes);
        }
    }

    /// Everything the client wrote so far.
    pub fn written(&self) -> Vec<u8> {
        self.written.lock().unwrap().clone()
    }

    /// Whether the client has written `needle` to its terminal.
    pub fn has_written(&self, needle: &[u8]) -> bool {
        let written = self.written.lock().unwrap();
        written.windows(needle.len()).any(|w| w == needle)
    }

    /// The first `n` bytes the client wrote, in hex, for a failure to say.
    pub fn head_hex(&self, n: usize) -> String {
        self.written()
            .iter()
            .take(n)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The client's pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The client's own pid, which this struct started: never a name.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Reads what the client writes until told to stop or the terminal hangs up, and answers its questions.
fn read_loop(
    mut side: std::fs::File,
    master: &Mutex<std::fs::File>,
    written: &Mutex<Vec<u8>>,
    mode: &AtomicU8,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 4096];
    // Bytes not yet answered for: a question can arrive split across two reads.
    let mut pending: Vec<u8> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let mut fds = libc::pollfd {
            fd: side.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd of a descriptor this thread holds.
        let ready = unsafe { libc::poll(&mut fds, 1, 100) };
        if ready <= 0 {
            continue;
        }
        let n = match side.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        written.lock().unwrap().extend_from_slice(&buf[..n]);
        pending.extend_from_slice(&buf[..n]);
        let mut answers: Vec<u8> = Vec::new();
        let mut cut = 0;
        for at in 0..pending.len() {
            let rest = &pending[at..];
            if rest.starts_with(ASK_MODE) {
                if let Some(bytes) = report(Mode::from_code(mode.load(Ordering::SeqCst))) {
                    answers.extend_from_slice(bytes);
                }
                cut = at + ASK_MODE.len();
            } else if let Some((ask, said)) = DEVICE_QUESTIONS
                .iter()
                .find(|(ask, _)| rest.starts_with(ask))
            {
                answers.extend_from_slice(said);
                cut = at + ask.len();
            }
        }
        if cut > 0 {
            pending.drain(..cut);
        }
        // What cannot start a question any more is dropped, so the buffer stays small.
        if pending.len() > 64 {
            pending.drain(..pending.len() - 16);
        }
        if !answers.is_empty() {
            let _ = master.lock().unwrap().write_all(&answers);
        }
    }
}
