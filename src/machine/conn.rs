//! The app's side of a connection to one machine: when it counts as quiet, when it is tried again
//! after it drops, and the thread that holds it.
//!
//! The pure part first ([`Timing`], [`Liveness`]): no clock is read here, so a test walks the whole
//! backoff with instants it makes up.

use std::io::{BufRead, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::paths::Paths;

use super::ssh::{self, Mode, Ssh};
use super::wire::{self, Down, Hello, LineError, Up};
use super::words::{self, Heard, Outcome};
use super::{Machine, clean, files};

/// The numbers of the connection, which are herdr's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// A connection that has said nothing for this long is dropped (three pings missed).
    pub quiet: Duration,
    /// How often the app says it is looking (the lease line, and the walk with it).
    pub lease_every: Duration,
    /// How long to wait before trying again when something outside taw has to change first: no taw
    /// there, no machines in it, a newer one, a login or a host key, and a feed that sent something
    /// taw does not read.
    pub needs_you_retry: Duration,
    /// The longest wait between two tries of a machine that keeps dropping.
    pub max_wait: Duration,
    /// How long a connection has to last for a later drop to start again from the short waits.
    pub settle: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            quiet: Duration::from_secs(15),
            lease_every: Duration::from_secs(5),
            needs_you_retry: Duration::from_secs(30),
            max_wait: Duration::from_secs(120),
            settle: Duration::from_secs(60),
        }
    }
}

impl Timing {
    /// The wait before try number `attempt` after a drop: none for the first, then 1, 2, 4, 8 seconds
    /// and so on, doubling up to [`Timing::max_wait`].
    fn backoff(&self, attempt: u32) -> Duration {
        match attempt {
            0 => Duration::ZERO,
            n => Duration::from_secs(1u64 << (n - 1).min(20)).min(self.max_wait),
        }
    }
}

/// What the app knows of the health of one machine's connection.
#[derive(Clone, Debug)]
pub struct Liveness {
    /// When the connection that is up was started.
    started: Option<Instant>,
    /// When something was last heard on it.
    last_line: Option<Instant>,
    /// How many drops in a row have waited, which is how far along the backoff is.
    attempt: u32,
    next_try: Instant,
}

impl Liveness {
    /// A machine not tried yet: the first try is now.
    pub fn new(now: Instant) -> Liveness {
        Liveness {
            started: None,
            last_line: None,
            attempt: 0,
            next_try: now,
        }
    }

    /// A connection was started (ssh was run): the quiet time counts from now.
    pub fn on_connect(&mut self, now: Instant) {
        self.started = Some(now);
        self.last_line = Some(now);
    }

    /// A line of any kind was heard.
    pub fn on_line(&mut self, now: Instant) {
        self.last_line = Some(now);
    }

    /// Whether the connection has said nothing for [`Timing::quiet`]. A connection that is not up is
    /// not quiet: it is not there.
    pub fn is_quiet(&self, now: Instant, t: &Timing) -> bool {
        self.last_line
            .is_some_and(|last| now.saturating_duration_since(last) >= t.quiet)
    }

    /// When the connection that is up becomes quiet, if nothing is heard before: `None` when none is up.
    pub fn quiet_at(&self, t: &Timing) -> Option<Instant> {
        self.last_line.map(|last| last + t.quiet)
    }

    /// When the next try is due.
    pub fn next_try(&self) -> Instant {
        self.next_try
    }

    /// The connection ended, in the way `outcome` says: sets when the next try is. The ones that need
    /// the user to do something, and a machine that sent what taw does not read, wait the long time and
    /// do not move the backoff; every other waits as far along the backoff as the drops in a row, which
    /// start again from none when this connection lasted [`Timing::settle`].
    pub fn on_drop(&mut self, now: Instant, outcome: &Outcome, t: &Timing) {
        if outcome.needs_you() || *outcome == Outcome::Garbage {
            self.on_drop_waiting(now, t);
            return;
        }
        if self.lasted(now, t) {
            self.attempt = 0;
        }
        self.started = None;
        self.last_line = None;
        self.next_try = now + t.backoff(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
    }

    /// The connection ended and what is wrong is not for the backoff to mend: the long wait, as for
    /// the outcomes that need the user (a machine that turned out to be this machine's own server).
    pub fn on_drop_waiting(&mut self, now: Instant, t: &Timing) {
        self.started = None;
        self.last_line = None;
        self.next_try = now + t.needs_you_retry;
    }

    /// The list was shown: a machine that waits tries now.
    pub fn wake(&mut self, now: Instant) {
        if self.next_try > now {
            self.next_try = now;
        }
    }

    fn lasted(&self, now: Instant, t: &Timing) -> bool {
        self.started
            .is_some_and(|started| now.saturating_duration_since(started) >= t.settle)
    }
}

/// The longest line the app reads from a feed, in bytes without its newline.
const MAX_FEED_LINE: usize = 1024 * 1024;

/// The most panes a state may hold: past it the machine is sending something taw does not read.
const MAX_PANES: usize = 5000;

/// How long a child that was ended is waited for.
const REAP: Duration = Duration::from_secs(2);

/// How long a child is given to end after SIGTERM, before it is killed.
const TERM_GRACE: Duration = Duration::from_millis(300);

/// How long a connection waits for a feed that has closed its output to end by itself, before ending it.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// How long the helper threads of an ended connection are waited for.
const HELPERS: Duration = Duration::from_secs(1);

/// How long dropping a [`ConnHandle`] waits for its thread.
const JOIN: Duration = Duration::from_secs(5);

/// How much of what ssh says on standard error is kept, from the end.
const STDERR_KEPT: usize = 4096;

/// What an app needs to hold a connection.
#[derive(Clone, Debug)]
pub struct ConnCtx {
    pub ssh: Ssh,
    /// This app's paths: the control sockets of ssh live under its state folder.
    pub paths: Paths,
    /// `<host>-<pid>` of this app: what the lease lines say.
    pub holder: String,
    pub timing: Timing,
    /// This machine's own tmux server as the hello says it (`"<pid> <start>"`), when one runs.
    pub local_server: Option<String>,
}

/// The holder word of this app: `<host>-<pid>`.
pub fn holder_word() -> String {
    format!("{}-{}", super::feed::host_name(), std::process::id())
}

/// Whether the machine that said `hello` is the tmux server of this machine: a machine of `[machines]`
/// that is this one itself, reached over ssh. Two servers that both say nothing are not the same.
pub fn is_this_server(local: Option<&str>, hello: &Hello) -> bool {
    !hello.server.is_empty() && local == Some(hello.server.as_str())
}

/// What a connection tells the app.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnEvent {
    /// The machine's state, cleaned and its rows with a bad id dropped.
    State {
        machine: usize,
        state: wire::State,
    },
    /// What the header says of the machine, and whether it is connected.
    Words {
        machine: usize,
        live: bool,
        words: String,
    },
    Toast {
        machine: usize,
        toast: wire::Toast,
    },
    Left {
        machine: usize,
        left: wire::Left,
    },
    /// A hello: the first of a connection, and any later one (the machine's tmux server came or went).
    Hello {
        machine: usize,
        hello: Hello,
    },
}

impl ConnEvent {
    /// The number of the connection that said it: the one it was started with.
    pub fn machine(&self) -> usize {
        match self {
            ConnEvent::State { machine, .. }
            | ConnEvent::Words { machine, .. }
            | ConnEvent::Toast { machine, .. }
            | ConnEvent::Left { machine, .. }
            | ConnEvent::Hello { machine, .. } => *machine,
        }
    }

    /// Says the event is of machine number `to` instead.
    pub fn set_machine(&mut self, to: usize) {
        match self {
            ConnEvent::State { machine, .. }
            | ConnEvent::Words { machine, .. }
            | ConnEvent::Toast { machine, .. }
            | ConnEvent::Left { machine, .. }
            | ConnEvent::Hello { machine, .. } => *machine = to,
        }
    }
}

/// What the reader of a feed's output found.
enum Item {
    Down(Down),
    /// Something taw does not read: not JSON, too long, a first line that is not a hello, too many panes.
    Bad,
    /// The output ended.
    Eof,
}

/// What a connection's thread is told.
enum Input {
    Up(Up),
    Wake,
    Stop,
    /// From the reader of connection number `serial`: a line from an earlier connection is stale.
    Feed {
        serial: u64,
        item: Item,
    },
}

/// A connection to one machine, held by a thread of its own. Dropping it (or [`ConnHandle::stop`]) ends
/// the thread, the ssh child it started and every helper of it.
pub struct ConnHandle {
    input: Sender<Input>,
    heard: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    done: Receiver<()>,
}

/// Starts a connection to `m`, number `index` in the app's list: events arrive on `tx`.
pub fn spawn(ctx: ConnCtx, index: usize, m: Machine, tx: Sender<ConnEvent>) -> ConnHandle {
    spawn_since(ctx, index, m, tx, 0)
}

/// [`spawn`] for a machine last heard from at `last_seen_ms` (unix milliseconds; 0 for never), as an
/// earlier run of the app wrote it in the machine's file: what the header says of how long it is.
pub(crate) fn spawn_since(
    ctx: ConnCtx,
    index: usize,
    m: Machine,
    tx: Sender<ConnEvent>,
    last_seen_ms: u64,
) -> ConnHandle {
    let (input, rx) = channel();
    let heard = Arc::new(AtomicU64::new(last_seen_ms));
    let (done_tx, done) = channel::<()>();
    let conn = Conn {
        live: Liveness::new(Instant::now()),
        name: m.name.clone(),
        ctx,
        index,
        machine: m,
        events: tx,
        input: input.clone(),
        rx,
        heard: Arc::clone(&heard),
        walk: None,
        serial: 0,
        gone: false,
    };
    let thread = std::thread::Builder::new()
        .name(format!("taw-conn-{}", conn.name))
        .spawn(move || {
            conn.run();
            drop(done_tx);
        })
        .map_err(|err| log::warn!("could not start the connection thread: {err}"))
        .ok();
    ConnHandle {
        input,
        heard,
        thread,
        done,
    }
}

impl ConnHandle {
    /// Sends a line to the machine's feed when it is connected. A walk is kept and sent again with
    /// every lease line, and once the connection is up again.
    pub fn send(&self, line: Up) {
        let _ = self.input.send(Input::Up(line));
    }

    /// Tries the machine now, if it is waiting to try again.
    pub fn wake(&self) {
        let _ = self.input.send(Input::Wake);
    }

    /// Unix milliseconds of the last line of any kind heard from the machine (0: none yet).
    pub fn heard_ms(&self) -> u64 {
        self.heard.load(Ordering::Relaxed)
    }

    /// Ends the connection, its thread and its ssh.
    pub fn stop(self) {
        drop(self);
    }

    /// Tells the thread to end, without waiting for it: a caller with several ends them at once, then
    /// drops them.
    pub(crate) fn request_stop(&self) {
        let _ = self.input.send(Input::Stop);
    }
}

impl Drop for ConnHandle {
    fn drop(&mut self) {
        self.request_stop();
        let Some(thread) = self.thread.take() else {
            return;
        };
        // The thread ends the child with a bounded wait of its own, so this does not wait for long.
        match self.done.recv_timeout(JOIN) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                log::warn!("a connection did not end within {JOIN:?}");
            }
        }
    }
}

/// A thread that signals its end by dropping its sender.
struct Worker {
    done: Receiver<()>,
}

impl Worker {
    /// Starts `work` on a thread; the returned worker says when it has ended.
    fn start(name: &str, work: impl FnOnce() + Send + 'static) -> Worker {
        let (tx, done) = channel::<()>();
        let started = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                work();
                drop(tx);
            });
        if started.is_err() {
            log::warn!("could not start the thread {name}");
        }
        Worker { done }
    }

    /// Waits up to `bound` for the thread to end.
    fn wait(&self, bound: Duration) {
        let _ = self.done.recv_timeout(bound);
    }

    /// Waits for every worker, all within one `bound` between them: each gets what is left of it, so that
    /// three that never end cost `bound` and not three times it.
    fn wait_all<'a>(workers: impl Iterator<Item = &'a Worker>, bound: Duration) {
        let until = Instant::now() + bound;
        for worker in workers {
            worker.wait(until.saturating_duration_since(Instant::now()));
        }
    }
}

/// How one try ended, before it is told as words.
enum End {
    /// The app ended it.
    Stop,
    /// The output closed.
    Eof,
    /// Nothing was heard for the quiet time.
    Quiet,
    /// Something taw does not read.
    Bad,
    /// A hello that is not protocol 1.
    Hello(Hello),
    /// The machine is this machine's own tmux server.
    ThisServer,
}

/// Whether the thread goes on.
#[derive(PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

/// The state of one machine's connection thread.
struct Conn {
    ctx: ConnCtx,
    index: usize,
    name: String,
    machine: Machine,
    events: Sender<ConnEvent>,
    /// To hand to the readers.
    input: Sender<Input>,
    rx: Receiver<Input>,
    heard: Arc<AtomicU64>,
    live: Liveness,
    /// The last walk the app sent, said again with every lease line.
    walk: Option<wire::WalkLine>,
    serial: u64,
    /// The app is not listening any more.
    gone: bool,
}

/// `line` as it is written: the object and a newline.
fn render_up(line: &Up) -> String {
    let mut text = serde_json::to_string(line).unwrap_or_default();
    text.push('\n');
    text
}

fn ceil_secs(wait: Duration) -> u64 {
    wait.as_millis().div_ceil(1000).max(1) as u64
}

impl Conn {
    fn run(mut self) {
        while self.wait_for_turn() {
            if self.attempt() == Flow::Stop {
                return;
            }
        }
    }

    fn event(&mut self, event: ConnEvent) {
        if self.events.send(event).is_err() {
            self.gone = true;
        }
    }

    /// Waits until the machine is due to be tried, taking what the app says meanwhile. `false` when the
    /// app has ended the connection.
    fn wait_for_turn(&mut self) -> bool {
        loop {
            let received = match self.rx.try_recv() {
                Ok(input) => Ok(input),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    let wait = self
                        .live
                        .next_try()
                        .saturating_duration_since(Instant::now());
                    if self.gone {
                        return false;
                    }
                    if wait.is_zero() {
                        return true;
                    }
                    self.rx.recv_timeout(wait)
                }
            };
            match received {
                Ok(Input::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Input::Wake) => self.live.wake(Instant::now()),
                Ok(Input::Up(Up::Walk(walk))) => self.walk = Some(walk),
                Ok(Input::Up(Up::Lease(_)) | Input::Feed { .. })
                | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Unix milliseconds now, for what goes in a file.
    fn heard_now(&self) {
        self.heard.store(files::now_ms(), Ordering::Relaxed);
    }

    /// Seconds since the machine was last heard from, when it ever was.
    fn seen_secs(&self) -> Option<u64> {
        match self.heard.load(Ordering::Relaxed) {
            0 => None,
            at => Some(files::now_ms().saturating_sub(at) / 1000),
        }
    }

    /// One try: starts the feed, holds the connection until it ends, tells how it ended.
    fn attempt(&mut self) -> Flow {
        self.serial += 1;
        let serial = self.serial;
        let timing = self.ctx.timing;
        let mut cmd = ssh::command(
            &self.ctx.ssh,
            &self.ctx.paths,
            &self.machine,
            Mode::Feed,
            &["machine", "feed", "--protocol", "1"],
        );
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let said = ssh::plain(&format!("ssh could not be run ({err})"));
                let outcome = words::classify(None, &said, Heard::Nothing);
                return self.dropped(&outcome);
            }
        };
        self.live.on_connect(Instant::now());
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        // From here the child is ended by the guard if this thread unwinds.
        let mut child = EndOnDrop(Some(child));
        let stderr_kept: Arc<Mutex<Vec<u8>>> = Arc::default();
        let readers = [
            stdout.map(|out| {
                let input = self.input.clone();
                Worker::start("taw-conn-out", move || read_feed(out, serial, &input))
            }),
            stderr.map(|err| {
                let kept = Arc::clone(&stderr_kept);
                Worker::start("taw-conn-err", move || keep_stderr(err, &kept))
            }),
        ];
        let (lines, lines_rx) = channel::<String>();
        let writer =
            stdin.map(|stdin| Worker::start("taw-conn-in", move || write_lines(stdin, &lines_rx)));

        let mut hello: Option<Hello> = None;
        let mut next_lease = Instant::now();
        let end = loop {
            if self.gone {
                break End::Stop;
            }
            let now = Instant::now();
            if self.live.is_quiet(now, &timing) {
                break End::Quiet;
            }
            let mut until = self.live.quiet_at(&timing).unwrap_or(now + timing.quiet);
            if hello.is_some() {
                until = until.min(next_lease);
            }
            match self.rx.recv_timeout(until.saturating_duration_since(now)) {
                Ok(Input::Stop) | Err(RecvTimeoutError::Disconnected) => break End::Stop,
                Ok(Input::Wake) => {}
                Ok(Input::Up(up)) => {
                    if let Up::Walk(walk) = &up {
                        self.walk = Some(walk.clone());
                    }
                    if hello.is_some() {
                        let _ = lines.send(render_up(&up));
                    }
                }
                Ok(Input::Feed { serial: from, .. }) if from != serial => {}
                Ok(Input::Feed { item, .. }) => match item {
                    Item::Eof => break End::Eof,
                    Item::Bad => break End::Bad,
                    Item::Down(line) => {
                        let now = Instant::now();
                        self.live.on_line(now);
                        self.heard_now();
                        match line {
                            Down::Hello(said) => {
                                self.event(ConnEvent::Hello {
                                    machine: self.index,
                                    hello: said.clone(),
                                });
                                if said.protocol != wire::PROTOCOL {
                                    break End::Hello(said);
                                }
                                if is_this_server(self.ctx.local_server.as_deref(), &said) {
                                    break End::ThisServer;
                                }
                                let first = hello.is_none();
                                let live_words = words::connected(&said.taw);
                                hello = Some(said);
                                self.event(ConnEvent::Words {
                                    machine: self.index,
                                    live: true,
                                    words: live_words,
                                });
                                if first {
                                    self.say_lease(&lines);
                                    next_lease = now + timing.lease_every;
                                }
                            }
                            Down::State(state) => self.event(ConnEvent::State {
                                machine: self.index,
                                state,
                            }),
                            Down::Toast(toast) => self.event(ConnEvent::Toast {
                                machine: self.index,
                                toast,
                            }),
                            Down::Left(left) => self.event(ConnEvent::Left {
                                machine: self.index,
                                left,
                            }),
                            Down::Ping => {}
                        }
                    }
                },
                Err(RecvTimeoutError::Timeout) => {}
            }
            // After every item, and not only when the wait ran out: an input that never empties (the app
            // sending lines faster than the tick) would otherwise hold the lease back until it did.
            let now = Instant::now();
            if hello.is_some() && now >= next_lease {
                self.say_lease(&lines);
                next_lease = now + timing.lease_every;
            }
        };

        // The end: stdin closes, the child is ended (waited for when it is ending by itself), and every
        // helper is waited for, all within one bound. A ProxyCommand that outlived a killed ssh can hold
        // ssh's standard error open, so a reader thread (and its descriptor) can outlive the reconnect
        // until that process ends: the accepted cost of the feed's ssh having no process group of its own.
        drop(lines);
        let exit = child.end(matches!(end, End::Eof));
        Worker::wait_all(readers.iter().flatten().chain(writer.iter()), HELPERS);
        let said = stderr_kept
            .lock()
            .map(|kept| ssh::last_line(&kept))
            .unwrap_or_default();
        let outcome = match end {
            End::Stop => return Flow::Stop,
            End::ThisServer => {
                self.live.on_drop_waiting(Instant::now(), &timing);
                let words = words::this_server(&self.name);
                self.event(ConnEvent::Words {
                    machine: self.index,
                    live: false,
                    words,
                });
                return if self.gone { Flow::Stop } else { Flow::Go };
            }
            End::Hello(said_hello) => words::classify(exit, &said, Heard::Hello(&said_hello)),
            End::Bad => words::classify(None, &said, Heard::Other),
            End::Quiet => Outcome::NotAnswering,
            End::Eof if hello.is_some() => Outcome::NotAnswering,
            End::Eof => words::classify(exit, &said, Heard::Nothing),
        };
        self.dropped(&outcome)
    }

    /// The lease line, and the walk the app last sent with it.
    fn say_lease(&self, lines: &Sender<String>) {
        let lease = Up::Lease(wire::LeaseLine {
            holder: self.ctx.holder.clone(),
        });
        let _ = lines.send(render_up(&lease));
        if let Some(walk) = &self.walk {
            let _ = lines.send(render_up(&Up::Walk(walk.clone())));
        }
    }

    /// The connection ended as `outcome` says: when it is tried again, and what the header says.
    fn dropped(&mut self, outcome: &Outcome) -> Flow {
        let now = Instant::now();
        self.live.on_drop(now, outcome, &self.ctx.timing);
        let retry = ceil_secs(self.live.next_try().saturating_duration_since(now));
        let text = match outcome {
            Outcome::NoTaw => words::no_taw(),
            Outcome::NoMachines => words::no_machines(""),
            Outcome::Newer { taw } => words::newer(taw, env!("CARGO_PKG_VERSION")),
            Outcome::Login => words::login(),
            Outcome::HostKey => words::host_key(&self.machine.ssh),
            Outcome::Garbage => words::garbage_header(),
            Outcome::Connected { .. } | Outcome::NotAnswering => {
                words::not_answering(self.seen_secs(), retry)
            }
        };
        self.event(ConnEvent::Words {
            machine: self.index,
            live: false,
            words: text,
        });
        if self.gone { Flow::Stop } else { Flow::Go }
    }
}

/// A child that is ended when this is dropped, so that a panic of the connection thread leaves no ssh
/// behind and unreaped. [`EndOnDrop::end`] ends it by choice and says how it exited.
struct EndOnDrop(Option<Child>);

impl EndOnDrop {
    fn end(&mut self, ending: bool) -> Option<i32> {
        self.0
            .take()
            .and_then(|mut child| end_child(&mut child, ending))
    }
}

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        self.end(false);
    }
}

/// Ends `child` and waits for it, with a bound: a child whose output has closed is given
/// [`EXIT_GRACE`] to end by itself (its exit status says why it ended), any other is ended by its pid
/// at once. `None` when it ended by a signal, or did not end within the bound.
fn end_child(child: &mut Child, ending: bool) -> Option<i32> {
    if ending && let Some(status) = reap(child, EXIT_GRACE) {
        return status.code();
    }
    // Its own pid, started by this connection: never by name. SIGTERM first, which is how ssh ends what
    // it started itself (a ProxyCommand); then a kill for one that did not end.
    // SAFETY: signals a child this connection started and has not yet waited for, so the pid is still its.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    if let Some(status) = reap(child, TERM_GRACE) {
        return status.code();
    }
    let _ = child.kill();
    reap(child, REAP).and_then(|status| status.code())
}

/// `child`'s exit status once it has ended within `bound`.
fn reap(child: &mut Child, bound: Duration) -> Option<std::process::ExitStatus> {
    let end = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Reads the feed's output, a line at a time, up to [`MAX_FEED_LINE`] bytes in one, and hands each over
/// as what it is: cleaned, and with the rows that are not tmux's dropped. Ends at the end of the output
/// or at the first thing taw does not read.
fn read_feed(out: ChildStdout, serial: u64, input: &Sender<Input>) {
    let mut reader = std::io::BufReader::new(out);
    let mut line: Vec<u8> = Vec::new();
    let mut first = true;
    let send = |item: Item| input.send(Input::Feed { serial, item }).is_ok();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_FEED_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => {
                send(Item::Eof);
                return;
            }
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            // Too long, or the end of the output in the middle of a line.
            send(if line.len() > MAX_FEED_LINE {
                Item::Bad
            } else {
                Item::Eof
            });
            return;
        }
        let parsed = std::str::from_utf8(&line)
            .map_err(|_| LineError::NotJson)
            .and_then(|text| wire::parse_down(text.trim()));
        let item = match parsed {
            Ok(Down::Hello(hello)) => Item::Down(Down::Hello(hello).cleaned()),
            // A feed starts with its hello.
            Ok(_) if first => Item::Bad,
            Ok(Down::State(state)) if state.panes.len() > MAX_PANES => Item::Bad,
            Ok(Down::State(state)) => match Down::State(state).cleaned() {
                Down::State(state) => Item::Down(Down::State(clean::state(state))),
                other => Item::Down(other),
            },
            // A toast's and a left's pane is a pane id of the far side, or nothing.
            Ok(down) => Item::Down(match down.cleaned() {
                Down::Toast(toast) => Down::Toast(clean::toast(toast)),
                Down::Left(left) => Down::Left(clean::left(left)),
                other => other,
            }),
            // A line of a newer taw whose kind is not known is skipped, once the feed has begun.
            Err(LineError::UnknownType(_)) if !first => continue,
            Err(_) => Item::Bad,
        };
        let bad = matches!(item, Item::Bad);
        first = false;
        if !send(item) || bad {
            return;
        }
    }
}

/// Keeps the last [`STDERR_KEPT`] bytes of what ssh says on standard error.
fn keep_stderr(mut err: ChildStderr, kept: &Mutex<Vec<u8>>) {
    let mut chunk = [0u8; 2048];
    loop {
        match err.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if let Ok(mut held) = kept.lock() {
                    held.extend_from_slice(&chunk[..n]);
                    if held.len() > STDERR_KEPT {
                        let extra = held.len() - STDERR_KEPT;
                        held.drain(..extra);
                    }
                }
            }
        }
    }
}

/// Writes each line to the feed's input until the lines stop or the feed has gone. On a thread of its
/// own, so that an input nobody reads (a full pipe) never keeps the connection from noticing it is quiet.
fn write_lines(mut stdin: ChildStdin, lines: &Receiver<String>) {
    while let Ok(line) = lines.recv() {
        if stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Drops a connection that lasted `lasted` and says how long until the next try.
    fn drop_after(
        live: &mut Liveness,
        now: &mut Instant,
        lasted: Duration,
        outcome: &Outcome,
    ) -> Duration {
        let t = Timing::default();
        live.on_connect(*now);
        *now += lasted;
        live.on_drop(*now, outcome, &t);
        live.next_try().saturating_duration_since(*now)
    }

    #[test]
    fn a_machine_is_this_machines_server_only_when_both_name_one_and_it_is_the_same() {
        let hello = |server: &str| Hello {
            server: server.to_string(),
            ..Hello::default()
        };
        assert!(is_this_server(
            Some("4211 1790000000"),
            &hello("4211 1790000000")
        ));
        assert!(!is_this_server(
            Some("4211 1790000000"),
            &hello("4211 1790000001")
        ));
        assert!(!is_this_server(Some("4211 1790000000"), &hello("")));
        assert!(!is_this_server(None, &hello("4211 1790000000")));
        // Two that say nothing are not one server.
        assert!(!is_this_server(Some(""), &hello("")));
        assert!(!is_this_server(None, &hello("")));
    }

    #[test]
    fn the_defaults_are_the_specs_numbers() {
        let t = Timing::default();
        assert_eq!(
            (
                t.quiet,
                t.lease_every,
                t.needs_you_retry,
                t.max_wait,
                t.settle
            ),
            (secs(15), secs(5), secs(30), secs(120), secs(60))
        );
    }

    #[test]
    fn helpers_that_never_end_cost_one_bound_between_them_and_not_one_each() {
        // Three workers that do not end: their senders are kept alive.
        let held: Vec<(Sender<()>, Worker)> = (0..3)
            .map(|_| {
                let (tx, done) = channel::<()>();
                (tx, Worker { done })
            })
            .collect();
        let started = Instant::now();
        Worker::wait_all(
            held.iter().map(|(_, worker)| worker),
            Duration::from_millis(500),
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(500), "{waited:?}");
        // One each would be 1 500 ms: the limit leaves 700 ms for the wakeups of a loaded machine.
        assert!(waited < Duration::from_millis(1200), "{waited:?}");
        // Workers that have ended cost nothing.
        let ended: Vec<Worker> = (0..3)
            .map(|_| {
                let (tx, done) = channel::<()>();
                drop(tx);
                Worker { done }
            })
            .collect();
        let started = Instant::now();
        Worker::wait_all(ended.iter(), Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn a_child_that_is_dropped_by_an_unwinding_thread_is_ended_and_reaped() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let thread = std::thread::spawn(move || {
            let _guard = EndOnDrop(Some(child));
            panic!("the connection thread panics");
        });
        assert!(thread.join().is_err());
        // Ended and reaped: asking for the pid says so (ECHILD), without signalling anything. A pid that
        // was reaped may already belong to another process, so only a child this test still owns is killed.
        let mut status = 0;
        // SAFETY: waitpid on a pid; with WNOHANG it never blocks.
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error().raw_os_error();
        if got == 0 {
            // Our child is still running: do not leave the sleep behind when the test fails.
            // SAFETY: the pid is of the child this test started and has not reaped.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            panic!("the child was left running");
        }
        assert!(
            got == -1 && error == Some(libc::ECHILD),
            "the child was ended but not reaped: waitpid said {got} (error {error:?})"
        );
    }

    #[test]
    fn a_machine_not_tried_yet_is_due_now() {
        let now = Instant::now();
        assert_eq!(Liveness::new(now).next_try(), now);
    }

    #[test]
    fn after_a_drop_the_wait_is_none_then_doubles_to_two_minutes() {
        let mut now = Instant::now();
        let mut live = Liveness::new(now);
        let waits: Vec<u64> = (0..11)
            .map(|_| drop_after(&mut live, &mut now, secs(1), &Outcome::NotAnswering).as_secs())
            .collect();
        assert_eq!(waits, [0, 1, 2, 4, 8, 16, 32, 64, 120, 120, 120]);
        // Far past where a shift by the attempt would overflow a u64: still the cap, and no panic.
        assert_eq!(Timing::default().backoff(100), secs(120));
        assert_eq!(Timing::default().backoff(u32::MAX), secs(120));
    }

    #[test]
    fn a_connection_that_lasted_a_minute_starts_the_waits_again_and_59_seconds_does_not() {
        let mut now = Instant::now();
        let mut live = Liveness::new(now);
        for _ in 0..4 {
            drop_after(&mut live, &mut now, secs(1), &Outcome::NotAnswering);
        }
        // The fifth drop would wait 8 seconds; after 59 connected seconds it still does.
        assert_eq!(
            drop_after(&mut live, &mut now, secs(59), &Outcome::NotAnswering),
            secs(8)
        );
        // After a whole minute it is at once, and the next waits are the short ones again.
        assert_eq!(
            drop_after(&mut live, &mut now, secs(60), &Outcome::NotAnswering),
            secs(0)
        );
        assert_eq!(
            drop_after(&mut live, &mut now, secs(1), &Outcome::NotAnswering),
            secs(1)
        );
    }

    #[test]
    fn a_drop_of_a_connection_never_started_does_not_reset() {
        let mut now = Instant::now();
        let mut live = Liveness::new(now);
        let t = Timing::default();
        for _ in 0..3 {
            drop_after(&mut live, &mut now, secs(1), &Outcome::NotAnswering);
        }
        now += secs(600);
        live.on_drop(now, &Outcome::NotAnswering, &t);
        assert_eq!(live.next_try().saturating_duration_since(now), secs(4));
    }

    #[test]
    fn each_outcome_that_needs_you_and_a_feed_nobody_reads_waits_30_seconds() {
        for outcome in [
            Outcome::NoTaw,
            Outcome::NoMachines,
            Outcome::Newer {
                taw: "9.0.0".into(),
            },
            Outcome::Login,
            Outcome::HostKey,
            Outcome::Garbage,
        ] {
            let mut now = Instant::now();
            let mut live = Liveness::new(now);
            assert_eq!(
                drop_after(&mut live, &mut now, secs(1), &outcome),
                secs(30),
                "{outcome:?}"
            );
            // Again and again: not faster, and not slower.
            assert_eq!(
                drop_after(&mut live, &mut now, secs(1), &outcome),
                secs(30),
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn a_wait_for_the_user_does_not_move_the_backoff() {
        let mut now = Instant::now();
        let mut live = Liveness::new(now);
        assert_eq!(
            drop_after(&mut live, &mut now, secs(1), &Outcome::NotAnswering),
            secs(0)
        );
        assert_eq!(
            drop_after(&mut live, &mut now, secs(1), &Outcome::NoTaw),
            secs(30)
        );
        assert_eq!(
            drop_after(&mut live, &mut now, secs(1), &Outcome::NotAnswering),
            secs(1)
        );
    }

    #[test]
    fn waking_makes_a_waiting_machine_due_now_and_leaves_a_due_one_alone() {
        let mut now = Instant::now();
        let mut live = Liveness::new(now);
        drop_after(&mut live, &mut now, secs(1), &Outcome::NoTaw);
        assert_eq!(live.next_try(), now + secs(30));
        now += secs(2);
        live.wake(now);
        assert_eq!(live.next_try(), now);
        let due = live.next_try();
        live.wake(now + secs(5));
        assert_eq!(live.next_try(), due);
    }

    #[test]
    fn a_connection_is_quiet_after_15_seconds_without_a_line_and_a_line_starts_the_count_again() {
        let t = Timing::default();
        let start = Instant::now();
        let mut live = Liveness::new(start);
        // Not up: not quiet.
        assert!(!live.is_quiet(start + secs(100), &t));
        live.on_connect(start);
        assert!(!live.is_quiet(start + Duration::from_millis(14_999), &t));
        assert!(live.is_quiet(start + secs(15), &t));
        live.on_line(start + secs(14));
        assert!(!live.is_quiet(start + secs(28), &t));
        assert!(live.is_quiet(start + secs(29), &t));
        // Dropped: not up again.
        live.on_drop(start + secs(29), &Outcome::NotAnswering, &t);
        assert!(!live.is_quiet(start + secs(500), &t));
    }
}
