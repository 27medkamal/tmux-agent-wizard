//! `taw machine feed`: what a machine says to the app that lists it, one JSON line at a time on
//! standard output ([`super::wire`]), and what it hears back on standard input.
//!
//! The app starts it through ssh and ends it by closing that. Standard output carries wire lines and
//! nothing else, each flushed as it is written; a failure is one short line on standard error. A write
//! to a standard output that has closed ends the feed quietly: the app has gone.
//!
//! Every tick the machine's tmux is read the way the popup reads it ([`crate::tui::data::build`]) and
//! the state is sent whole when it is not what was last sent. The feed never shows a notification,
//! plays a sound or runs a hook: it passes on what the machine's watcher wrote to the events file.

use std::io::{BufRead, Read, Write};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::paths::Paths;
use crate::session::worktree;
use crate::tmux::Tmux;
use crate::tui::data::{self, GitWorker};
use crate::tui::{GIT_TTL, TICK};

use super::files::{self, EventTail};
use super::wire::{self, Down, Hello, State, Up};

/// A feed with nothing to say for this long says `ping`, so the app can tell a quiet machine from a
/// lost connection.
pub const PING_AFTER: Duration = Duration::from_secs(5);

/// The longest line the app may send, in bytes without its newline; a longer one is skipped whole.
pub const MAX_LINE: usize = 64 * 1024;

/// What the feed was asked.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The protocol the app speaks. The hello says 1 whatever this is: the app reads the number and says
    /// which side to update.
    pub protocol: u32,
    /// Say hello and end.
    pub once: bool,
}

/// The line the feed's output goes through: whole, flushed, and `Err` once nobody reads.
struct Sink<W: Write> {
    out: W,
    last_sent: Instant,
}

impl<W: Write> Sink<W> {
    fn send(&mut self, line: &Down) -> Result<(), ()> {
        let text = wire::render(line);
        self.out
            .write_all(text.as_bytes())
            .and_then(|()| self.out.flush())
            .map_err(|_| ())?;
        self.last_sent = Instant::now();
        Ok(())
    }
}

/// This machine's name without its domain, as `gethostname` gives it.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for the length given, one less than its size, so the name is
    // ended by a zero whatever the system writes.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    short_host(&String::from_utf8_lossy(&buf[..end]))
}

/// A host name up to its first dot: `studio.local` is `studio`. A name that starts with a dot, or is
/// empty, is empty: the hello then says no host, and the app uses the name it gave the machine.
fn short_host(name: &str) -> String {
    name.split('.').next().unwrap_or_default().to_string()
}

/// `<tmux server pid> <server start unix seconds>`, or empty when no server answers. The one reader of
/// that word: the hello says it, and the app compares it with its own server's to tell that a machine of
/// `[machines]` is the machine it runs on.
pub fn server_word(tmux: &Tmux) -> String {
    let Ok(out) = tmux.run(&["display-message", "-p", "#{pid} #{start_time}"]) else {
        return String::new();
    };
    let mut words = out.split_whitespace();
    match (
        words.next().and_then(|w| w.parse::<u64>().ok()),
        words.next().and_then(|w| w.parse::<u64>().ok()),
        words.next(),
    ) {
        (Some(pid), Some(start), None) => format!("{pid} {start}"),
        _ => String::new(),
    }
}

fn hello_for(host: &str, server: String) -> Hello {
    Hello {
        taw: env!("CARGO_PKG_VERSION").to_string(),
        protocol: wire::PROTOCOL,
        host: host.to_string(),
        server,
    }
}

/// The hello to send now, when the tmux server is not the one the last hello named: one that came up
/// after the feed started, one that went away, and one that restarted between two ticks (its pid or start
/// time differs). `None` while it is the same.
fn hello_if_changed(said: &Hello, server: String, host: &str) -> Option<Hello> {
    (server != said.server).then(|| hello_for(host, server))
}

/// What the popup reads, as the wire has it. `None` when tmux could not be read at all.
fn read_state(tmux: &Tmux, cfg: &Config, paths: &Paths, git: &GitWorker) -> Option<State> {
    let snapshot = data::build(tmux, cfg, git, None, None).ok()?;
    // The records of worktrees no session is running: the popup's closed rows.
    let records = worktree::list(tmux, paths).unwrap_or_default();
    let closed = crate::tui::tree::closed_worktrees(&snapshot, &records);
    // The records a session is running, which the list does not draw: the app names a session's worktree
    // sessions from them when it asks before closing it.
    let open = crate::tui::tree::open_worktrees(&snapshot, &records);
    Some(State {
        spaces: snapshot.spaces.iter().map(wire::WireSpace::from).collect(),
        panes: snapshot.panes.iter().map(wire::WirePane::from).collect(),
        agents: snapshot.agents.iter().map(wire::WireAgent::from).collect(),
        worktrees: closed
            .iter()
            .filter_map(|(_, record)| records.get(*record))
            .map(wire::WireWorktree::from)
            .collect(),
        open_worktrees: open
            .iter()
            .filter_map(|record| records.get(*record))
            .map(wire::WireWorktree::from)
            .collect(),
    })
}

/// What decides whether a state is news: a copy of the state with each agent's title cut the way the
/// list draws it, without the glyph an agent puts in front of it, since an agent's spinner turns many
/// times a second and is no change. Only the comparison strips: what goes on the wire is the state as it
/// was read, with the raw title (the glyph of the frame that was current at the last real change).
fn comparable(state: &State) -> State {
    let mut state = state.clone();
    for agent in &mut state.agents {
        agent.pane_title = data::strip_title_glyph(&agent.pane_title);
    }
    state
}

/// Reads the app's lines from `input` until it ends, acting on each, and says when it has (`ended`).
fn listen(input: impl Read, paths: Paths, holder: Arc<Mutex<String>>, ended: impl FnOnce()) {
    let mut reader = std::io::BufReader::new(input);
    let mut line: Vec<u8> = Vec::new();
    // Says that the app this feed speaks for is still there, from its first lease line until this ends (the app
    // ended, and its ssh with it): then its lease is given up, or, when the process is killed, the lock goes with
    // it, and the next app takes the lease at once ([`files::offer_lease`]).
    let mut alive: Option<files::HolderLock> = None;
    'lines: loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line);
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            if line.len() > MAX_LINE {
                // Too long: skip to the end of it.
                let mut rest = Vec::new();
                loop {
                    rest.clear();
                    match (&mut reader)
                        .take(MAX_LINE as u64)
                        .read_until(b'\n', &mut rest)
                    {
                        Ok(0) | Err(_) => break 'lines,
                        Ok(_) if rest.last() == Some(&b'\n') => break,
                        Ok(_) => {}
                    }
                }
            }
            // The end of the input with a line half said: nothing to act on.
            else {
                break;
            }
            continue;
        }
        // A whole line is at most MAX_LINE bytes and its newline: the read above stopped there.
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        match wire::parse_up(text.trim()) {
            Ok(Up::Lease(lease)) if !lease.holder.is_empty() => {
                if let Ok(mut last) = holder.lock() {
                    last.clone_from(&lease.holder);
                }
                if alive
                    .as_ref()
                    .is_none_or(|lock| lock.holder() != lease.holder)
                {
                    // The lock of the holder heard before is let go first, which gives its lease up.
                    drop(alive.take());
                    alive = files::take_holder_lock(&paths, &lease.holder);
                }
                files::offer_lease(&paths, &lease.holder, files::now_ms());
            }
            Ok(Up::Walk(walk)) => {
                // Whose walk it is: the holder of the lease this feed last heard (the one `lease_is_held` compares),
                // so that a key on this machine takes a walk only from the app that holds the lease.
                let heard = holder.lock().map(|last| last.clone()).unwrap_or_default();
                files::write_walk(&paths, &heard, &walk.you, &walk.stops, files::now_ms());
            }
            _ => {}
        }
    }
    // The lock is let go (its file removed, the lease given up) before the end is said: the process ends as soon as
    // it hears, and the lock's own drop would then never run.
    drop(alive);
    ended();
}

/// Whether the app this feed last heard a lease from holds a fresh lease now.
fn lease_is_held(paths: &Paths, holder: &Mutex<String>) -> bool {
    let Ok(last) = holder.lock() else {
        return false;
    };
    !last.is_empty()
        && files::read_lease(paths).is_some_and(|lease| lease.holder == *last)
        && files::lease_fresh(paths, files::now_ms())
}

/// The longest the feed goes without looking at the events file while it waits out a tick: a `left` line that
/// carries `go` is waited for by the app for half a second only ([`crate::tui::app_screen`]'s `LEFT_WAIT`), so a
/// line must not sit behind a whole tick (and the state read that ends it).
const EVENTS_SLICE: Duration = Duration::from_millis(50);

/// Runs the feed: says hello, then every tick says what changed, until `input` ends or `out` closes.
pub fn run<W: Write>(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    options: Options,
    input: impl Read + Send + 'static,
    out: W,
) -> anyhow::Result<()> {
    run_ticking(tmux, cfg, paths, options, input, out, TICK)
}

/// What the feed passes on from the events file (toasts, with their `lease` mark, and `left` lines), sent as they
/// are read. `Err` once nobody reads.
fn relay_events<W: Write>(
    sink: &mut Sink<W>,
    events: &mut EventTail,
    paths: &Paths,
    holder: &Mutex<String>,
) -> Result<(), ()> {
    for event in events.read_new() {
        let line = match event {
            Down::Toast(mut toast) => {
                toast.lease = lease_is_held(paths, holder);
                Down::Toast(toast)
            }
            left @ Down::Left(_) => left,
            _ => continue,
        };
        sink.send(&line)?;
    }
    Ok(())
}

/// [`run`] with the length of a tick given: tests make it long, to show that nothing waits for it.
fn run_ticking<W: Write>(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    options: Options,
    input: impl Read + Send + 'static,
    out: W,
    tick_length: Duration,
) -> anyhow::Result<()> {
    let host = host_name();
    let mut sink = Sink {
        out,
        last_sent: Instant::now(),
    };
    let mut said = hello_for(&host, server_word(tmux));
    if sink.send(&Down::Hello(said.clone())).is_err() {
        return Ok(());
    }
    // A protocol this taw does not speak is answered with the hello alone: the app reads the number.
    if options.once || options.protocol != wire::PROTOCOL {
        return Ok(());
    }

    let holder = Arc::new(Mutex::new(String::new()));
    let (ended_tx, ended_rx) = channel();
    {
        let (paths, holder) = (paths.clone(), Arc::clone(&holder));
        std::thread::Builder::new()
            .name("taw-feed-in".to_string())
            .spawn(move || {
                listen(input, paths, holder, move || {
                    let _ = ended_tx.send(());
                })
            })?;
    }

    let git = GitWorker::new(GIT_TTL);
    let mut events = EventTail::at_end(paths);
    let mut last: Option<State> = None;
    loop {
        let tick = Instant::now();
        // Who the server is, every tick: a hello says so whenever it is not the one last named, before
        // the state that belongs to it.
        if let Some(now) = hello_if_changed(&said, server_word(tmux), &host) {
            said = now;
            if sink.send(&Down::Hello(said.clone())).is_err() {
                return Ok(());
            }
        }
        // The events first: what the machine's watcher wrote since the last look goes out before the state is read,
        // which takes tmux calls and git.
        if relay_events(&mut sink, &mut events, paths, &holder).is_err() {
            return Ok(());
        }
        let state = match read_state(tmux, cfg, paths, &git) {
            Some(state) => Some(state),
            // No server to ask: an empty state, and a look again next tick. A server that answers
            // `display-message` but not the listings keeps the last state.
            None if tmux.server_start_unix().is_none() => Some(State::default()),
            None => None,
        };
        if let Some(state) = state
            && send_if_new(&mut sink, &mut last, state).is_err()
        {
            return Ok(());
        }

        if sink.last_sent.elapsed() >= PING_AFTER && sink.send(&Down::Ping).is_err() {
            return Ok(());
        }

        // Wait out the rest of the tick in slices, looking at the events file after each (nothing else is
        // read); the end of the input ends the feed at once.
        loop {
            let rest = tick_length.saturating_sub(tick.elapsed());
            match ended_rx.recv_timeout(rest.min(EVENTS_SLICE)) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if tick.elapsed() >= tick_length {
                break;
            }
            if relay_events(&mut sink, &mut events, paths, &holder).is_err() {
                return Ok(());
            }
        }
    }
}

/// Sends `state` unless it is what was last sent, in the sense of [`comparable`].
fn send_if_new<W: Write>(
    sink: &mut Sink<W>,
    last: &mut Option<State>,
    state: State,
) -> Result<(), ()> {
    let key = comparable(&state);
    if last.as_ref() == Some(&key) {
        return Ok(());
    }
    sink.send(&Down::State(state))?;
    *last = Some(key);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::data::AgentRow;

    fn state_with_title(title: &str) -> State {
        State {
            agents: vec![wire::WireAgent::from(&AgentRow {
                pane_title: title.to_string(),
                ..AgentRow::default()
            })],
            ..State::default()
        }
    }

    #[test]
    fn a_spinner_glyph_in_a_title_is_not_a_difference_and_a_word_is() {
        let a = comparable(&state_with_title("\u{280b} thinking"));
        let b = comparable(&state_with_title("\u{2819} thinking"));
        let c = comparable(&state_with_title("\u{2819} writing"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn a_state_is_sent_once_until_it_differs() {
        let mut sink = Sink {
            out: Vec::new(),
            last_sent: Instant::now(),
        };
        let mut last = None;
        let one = state_with_title("\u{280b} thinking");
        send_if_new(&mut sink, &mut last, one.clone()).unwrap();
        send_if_new(&mut sink, &mut last, state_with_title("\u{2819} thinking")).unwrap();
        assert_eq!(sink.out.iter().filter(|b| **b == b'\n').count(), 1);
        send_if_new(&mut sink, &mut last, state_with_title("\u{2819} writing")).unwrap();
        assert_eq!(sink.out.iter().filter(|b| **b == b'\n').count(), 2);
    }

    #[test]
    fn a_hello_is_sent_whenever_the_server_is_not_the_one_last_named() {
        let said = hello_for("studio", "4211 1790000000".to_string());
        assert_eq!(
            hello_if_changed(&said, "4211 1790000000".to_string(), "studio"),
            None
        );
        // A restart inside a tick: another pid, or the same pid and another start.
        let restarted = hello_if_changed(&said, "4212 1790000009".to_string(), "studio").unwrap();
        assert_eq!(restarted.server, "4212 1790000009");
        assert_eq!(restarted.host, "studio");
        assert!(hello_if_changed(&said, "4211 1790000009".to_string(), "studio").is_some());
        // Gone, and then back where it was nothing.
        let gone = hello_if_changed(&said, String::new(), "studio").unwrap();
        assert_eq!(gone.server, "");
        assert!(hello_if_changed(&gone, String::new(), "studio").is_none());
        assert!(hello_if_changed(&gone, "7 8".to_string(), "studio").is_some());
    }

    #[test]
    fn a_name_with_a_domain_is_cut_at_the_first_dot() {
        assert_eq!(short_host("studio.local"), "studio");
        assert_eq!(short_host("studio.a.b"), "studio");
        assert_eq!(short_host("studio"), "studio");
        assert_eq!(short_host(".x"), "");
        assert_eq!(short_host(""), "");
        assert!(!host_name().contains('.'));
    }

    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_to_a_closed_output_is_an_error_to_end_on_and_not_a_panic() {
        let mut sink = Sink {
            out: Closed,
            last_sent: Instant::now(),
        };
        assert!(sink.send(&Down::Ping).is_err());
    }

    /// The feed's output as lines, one per write.
    struct Lines(std::sync::mpsc::Sender<String>);

    impl Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An input that has nothing to say and ends when its sender is dropped.
    struct Hold(std::sync::mpsc::Receiver<()>);

    impl Read for Hold {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    /// An input that gives `chunks` one read each, then ends, and says at its end whether the holder's lock file
    /// and the lease are there: the feed still holds the lock then.
    struct Script {
        chunks: std::collections::VecDeque<Vec<u8>>,
        paths: Paths,
        at_end: std::rc::Rc<std::cell::Cell<Option<(usize, bool)>>>,
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some(mut chunk) = self.chunks.pop_front() else {
                self.at_end.set(Some((
                    holder_files(&self.paths),
                    files::read_lease(&self.paths).is_some(),
                )));
                return Ok(0);
            };
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            if n < chunk.len() {
                self.chunks.push_front(chunk.split_off(n));
            }
            Ok(n)
        }
    }

    /// How many lock files the holders folder has.
    fn holder_files(paths: &Paths) -> usize {
        std::fs::read_dir(paths.machines_dir().join("holders")).map_or(0, |dir| dir.count())
    }

    /// A feed whose app ended has given up the holder's lock, its file and the lease by the time it says it ended:
    /// the process ends as soon as that is heard, and what is left then stays on disk. Both ways in which the input
    /// ends: after a whole line, and inside a line too long to read.
    #[test]
    fn a_feed_that_ends_has_left_no_holder_file_and_no_lease_when_it_says_so() {
        let too_long = vec![b'x'; MAX_LINE + 10];
        for tail in [Vec::new(), too_long] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::rooted_at(dir.path().join("config"), dir.path().join("state"));
            let at_end = std::rc::Rc::new(std::cell::Cell::new(None));
            let input = Script {
                chunks: [
                    b"{\"type\":\"lease\",\"holder\":\"mac-1\"}\n".to_vec(),
                    tail.clone(),
                ]
                .into_iter()
                .filter(|chunk| !chunk.is_empty())
                .collect(),
                paths: paths.clone(),
                at_end: std::rc::Rc::clone(&at_end),
            };
            let mut when_said = None;
            listen(
                input,
                paths.clone(),
                Arc::new(Mutex::new(String::new())),
                || {
                    when_said = Some((holder_files(&paths), files::read_lease(&paths).is_some()));
                },
            );
            // The positive half: while the feed ran, it held the lock (one file) and the lease was there.
            assert_eq!(
                at_end.get(),
                Some((1, true)),
                "tail of {} bytes",
                tail.len()
            );
            assert_eq!(when_said, Some((0, false)), "tail of {} bytes", tail.len());
        }
    }

    /// A `left` line is passed on within a slice of the tick it came in, not at the next tick: with a tick of 5 s
    /// the line that was written after the first tick arrives within 2 s (a feed that looks at the events file once a
    /// tick could not pass this at any speed of machine). Whatever it passes on before it is a state or a hello.
    #[test]
    fn a_left_line_is_passed_on_within_a_slice_of_the_tick_it_came_in() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = crate::cli::test_support::stub_tmux(dir.path(), &Default::default());
        let ctx = crate::cli::test_support::stub_ctx(tmux);
        let (tmux, cfg, paths) = (&ctx.tmux, &ctx.cfg, &ctx.paths);
        let (lines_tx, lines) = channel::<String>();
        std::thread::scope(|scope| {
            let (hold_tx, hold_rx) = channel::<()>();
            scope.spawn(move || {
                run_ticking(
                    tmux,
                    cfg,
                    paths,
                    Options {
                        protocol: wire::PROTOCOL,
                        once: false,
                    },
                    Hold(hold_rx),
                    Lines(lines_tx),
                    Duration::from_secs(5),
                )
            });
            let mut seen = Vec::new();
            // The first tick is over when its state has been said: the events were looked at before it.
            let deadline = Instant::now() + Duration::from_secs(20);
            while !seen
                .iter()
                .any(|line: &String| line.contains(r#""type":"state""#))
            {
                match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(line) => seen.push(line),
                    Err(_) => panic!("no state said by the first tick, saw {seen:?}"),
                }
            }
            files::append_event(
                paths,
                &Down::Left(wire::Left {
                    holder: "mac-1".into(),
                    pane: "%7".into(),
                    go: Some("studio:%9".into()),
                }),
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut told = false;
            while !told {
                match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(line) => {
                        told = line.contains(r#""type":"left""#) && line.contains("studio:%9");
                        seen.push(line);
                    }
                    Err(_) => panic!(
                        "the left line was not passed on within 2 s of a 5 s tick, saw {seen:?}"
                    ),
                }
            }
            drop(hold_tx);
        });
    }
}
