//! The preview of a pane on another machine, drawn in the app.
//!
//! While the cursor sits on a live machine's pane, `taw pane read` is run on that machine (over the same ssh as
//! its other short verbs, [`bounded::run`]), on a thread of its own: a frame and a key never wait for it. At
//! most one read is in flight per machine, started at most once in half a tick. A reply is shown only for the
//! pane that is under the cursor when it arrives; one for another pane is dropped. A read that fails keeps the
//! last text, with its age, and says nothing.
//!
//! The text is another machine's output: it goes through [`clean::preview`] (colours and newlines only, so no
//! clipboard sequence, title or cursor move reaches this terminal) before anything parses it.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::text::{Line, Text};

use crate::machine::bounded::{self, Ender, Ran};
use crate::machine::ssh::{self, Mode, Ssh};
use crate::machine::{Machine, clean, split_id, valid_tmux_id};
use crate::paths::Paths;
use crate::tui::TICK;
use crate::tui::app::App;
use crate::tui::preview;

/// How long a read of a pane may take before its ssh is ended.
pub(super) const READ_BOUND: Duration = Duration::from_secs(8);

/// How much of a reply's text is looked at: a screen is a few kilobytes, so more is not a screen.
pub(super) const MAX_REPLY: usize = 64 * 1024;

/// How often a read in flight is looked at by the thread that runs it.
const LOOK: Duration = Duration::from_millis(30);

/// The arguments of the far verb for the pane `raw` (`%16`): its visible screen, with its colours.
/// `--source visible` because the default source is the pane's recent history, and `--ansi` because the
/// colours come only with it. Pinned by a test and by the end-to-end test, which runs the real far verb.
pub(super) fn read_args(raw: &str) -> [&str; 6] {
    ["pane", "read", raw, "--source", "visible", "--ansi"]
}

/// The text of a `pane read` reply: the last line of what the far side wrote is the envelope
/// (`{"id":..,"result":{"pane_id":..,"text":..}}`), a login shell may have said something before it. `None`
/// for anything else (an error envelope, a line that is no JSON, no output).
pub(super) fn reply_text(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let reply: serde_json::Value = serde_json::from_str(line).ok()?;
    reply["result"]["text"].as_str().map(str::to_string)
}

/// A reply's `text` as what the panel draws: cut at [`MAX_REPLY`] bytes (on a character), then cleaned to
/// colours and newlines ([`clean::preview`]), then parsed ([`preview::try_to_text`]); text that does not parse
/// is each line as plain text ([`clean::plain`]). Never the bytes as they came.
pub(super) fn shown(text: &str) -> Text<'static> {
    let mut end = text.len().min(MAX_REPLY);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cleaned = clean::preview(&text[..end]);
    preview::try_to_text(&cleaned).unwrap_or_else(|| {
        Text::from(
            cleaned
                .lines()
                .map(|line| Line::from(clean::plain(line)))
                .collect::<Vec<_>>(),
        )
    })
}

/// What starts a read of a raw pane id on a machine, and gives the channel its answer comes on. The flag is the
/// read's stop flag: once it is set the read ends its ssh by its pid and answers `None`.
pub(super) type Launch<'a> =
    dyn FnMut(&str, &str, Arc<Ender>) -> Option<Receiver<Option<String>>> + 'a;

/// How long dropping the preview waits for the reads it ended to say they are over (their ssh killed and
/// reaped): a few looks of the thread that runs a read, as a connection's end waits for its thread.
const DROP_WAIT: Duration = Duration::from_millis(300);

/// A read of one pane that has not answered yet.
struct Flight {
    /// The list's id of the pane asked for (`studio:%16`).
    pane: String,
    /// What the thread says when it is done: the reply's text, or `None` for any failure.
    reply: Receiver<Option<String>>,
    /// Set to end the read: its ssh is killed by its pid and reaped.
    stop: Arc<Ender>,
    /// When the read was started: a screen is never younger than this, so it is the capture time.
    began: Instant,
}

/// The reads in flight and when the shown pane was last read.
#[derive(Default)]
pub(super) struct RemotePreview {
    /// One per machine, by name.
    flights: HashMap<String, Flight>,
    /// The pane last read, and when the read began: the age of the text on screen is measured from it.
    got: Option<(String, Instant)>,
    /// When a read was last started.
    started: Option<Instant>,
}

impl Drop for RemotePreview {
    /// Kills and reaps every ssh in flight itself (the child is shared with the thread that runs it), then
    /// waits a bounded moment for the threads, so no ssh outlives the app.
    fn drop(&mut self) {
        for flight in self.flights.values() {
            flight.stop.end();
        }
        let deadline = Instant::now() + DROP_WAIT;
        for flight in self.flights.values() {
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = flight.reply.recv_timeout(left);
        }
    }
}

impl RemotePreview {
    /// One look at the preview of a pane on a machine, at `now`: the preview is switched to the pane under the
    /// cursor (its old text is not drawn under a new row), the replies that arrived are taken (only the cursor
    /// pane's is shown), a read is started when none is in flight for the machine, and the age is set.
    ///
    /// `launch(machine, raw, stop)` starts a read of the raw pane id on the machine and gives the channel its
    /// answer comes on, or nothing when none can be started. A read whose pane is not the cursor's (the cursor
    /// is on another pane, or on no machine's pane at all) is ended, on every call. Its slot is free once it has
    /// answered.
    pub(super) fn step(&mut self, app: &mut App, now: Instant, launch: &mut Launch) {
        let target = app.preview_remote_pane();
        let wanted = target.as_ref().map(|target| target.pane.as_str());
        // The text read last is for its pane only: coming back to it later has nothing to show an age of.
        if self
            .got
            .as_ref()
            .is_some_and(|(pane, _)| Some(pane.as_str()) != wanted)
        {
            self.got = None;
        }
        for flight in self.flights.values() {
            if Some(flight.pane.as_str()) != wanted {
                flight.stop.set();
            }
        }
        let mut fresh = None;
        self.flights
            .retain(|_, flight| match flight.reply.try_recv() {
                Ok(reply) => {
                    if Some(flight.pane.as_str()) == wanted {
                        fresh = reply.map(|text| (text, flight.began)).or(fresh.take());
                    }
                    false
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => false,
            });
        let Some(target) = target else {
            return;
        };
        if app.preview_shows() != Some(target.pane.as_str()) {
            app.set_preview(Some(target.pane.clone()), None);
        }
        if let Some((text, began)) = fresh {
            app.set_preview(Some(target.pane.clone()), Some(shown(&text)));
            self.got = Some((target.pane.clone(), began));
        }
        let (_, raw) = split_id(&target.pane);
        let due = self
            .started
            .is_none_or(|at| now.saturating_duration_since(at) >= TICK / 2);
        if app.show_preview
            && target.live
            && valid_tmux_id(raw)
            && due
            && !self.flights.contains_key(&target.machine)
            && let stop = Ender::new()
            && let Some(reply) = launch(&target.machine, raw, stop.clone())
        {
            self.started = Some(now);
            self.flights.insert(
                target.machine.clone(),
                Flight {
                    pane: target.pane.clone(),
                    reply,
                    stop,
                    began: now,
                },
            );
        }
        app.preview_age = self
            .got
            .as_ref()
            .filter(|(pane, _)| *pane == target.pane)
            .map(|(_, at)| now.saturating_duration_since(*at));
    }

    /// What `step` does when no read can be started (a test, or a cursor that is only following): the switch of
    /// the pane and the replies, without a launch.
    pub(super) fn follow(&mut self, app: &mut App, now: Instant) {
        self.step(app, now, &mut |_, _, _| None);
    }
}

/// Starts the read of `raw` on `machine`: the far verb over [`ssh::command`], run by [`launch_command`].
pub(super) fn launch(
    ssh: &Ssh,
    paths: &Paths,
    machine: &Machine,
    raw: &str,
    stop: Arc<Ender>,
) -> Option<Receiver<Option<String>>> {
    let command = ssh::command(ssh, paths, machine, Mode::Command, &read_args(raw));
    launch_command(command, stop)
}

/// Runs `command` by [`bounded::run`] on a thread of its own that sends the reply's text (or `None`) and ends,
/// within [`READ_BOUND`], or as soon as `stop` is set: then the process is ended by its pid and reaped first.
fn launch_command(command: Command, stop: Arc<Ender>) -> Option<Receiver<Option<String>>> {
    launch_command_with_look(command, stop, LOOK)
}

/// [`launch_command`] with the thread's look set (a test gives it a long one).
fn launch_command_with_look(
    command: Command,
    stop: Arc<Ender>,
    look: Duration,
) -> Option<Receiver<Option<String>>> {
    let (sent, reply) = mpsc::channel();
    // A thread the system refuses is no read (nothing ran), not a panic on a tick.
    std::thread::Builder::new()
        .spawn(move || {
            let ran = bounded::run_in(&stop, &mut (), command, READ_BOUND, look, |_| false, |_| {});
            let text = match ran {
                Ran::Done { stdout, .. } => reply_text(&stdout),
                Ran::Failed(_) | Ran::TimedOut | Ran::Ended => None,
            };
            let _ = sent.send(text);
        })
        .ok()?;
    Some(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::machine::MachineView;
    use crate::machine::wire::{State, WirePane, WireSpace};
    use crate::tui::fixtures::snapshot;
    use crate::tui::machines::merge;
    use crate::tui::theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    use std::sync::mpsc::Sender;

    fn view(name: &str, live: bool, panes: &[u32]) -> MachineView {
        MachineView {
            name: name.into(),
            live,
            state: Some(State {
                spaces: panes
                    .iter()
                    .map(|id| WireSpace {
                        session_id: format!("${id}"),
                        session_name: format!("s{id}"),
                        session_path: "~/c".into(),
                        windows: 1,
                        active_window_id: format!("@{id}"),
                        active_pane_id: Some(format!("%{id}")),
                        ..WireSpace::default()
                    })
                    .collect(),
                panes: panes
                    .iter()
                    .map(|id| WirePane {
                        pane_id: format!("%{id}"),
                        session_id: format!("${id}"),
                        window_id: format!("@{id}"),
                        window_index: 1,
                        window_name: "zsh".into(),
                        window_panes: 1,
                        pane_index: 1,
                        pane_current_command: "zsh".into(),
                        pane_active: true,
                        alternate_on: false,
                    })
                    .collect(),
                ..State::default()
            }),
            last_seen: Some(5_000),
            ..MachineView::default()
        }
    }

    fn app_over<'a>(cfg: &'a Config, views: &[MachineView]) -> App<'a> {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        App::new(cfg, theme, String::new(), merge(snapshot(), views, 5_000))
    }

    /// The cursor moved by keys to the row whose preview pane is `pane`.
    fn cursor_on(app: &mut App, pane: &str) {
        for _ in 0..80 {
            if app.preview_remote_pane().map(|r| r.pane).as_deref() == Some(pane) {
                return;
            }
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        panic!("the cursor never reached {pane}");
    }

    /// A launch that records what it was asked and keeps the sending ends, so a test says when a read answers.
    #[derive(Default)]
    struct Reads {
        asked: Vec<(String, String)>,
        ends: Vec<Sender<Option<String>>>,
        stops: Vec<Arc<Ender>>,
    }

    impl Reads {
        fn step(&mut self, remote: &mut RemotePreview, app: &mut App, now: Instant) {
            remote.step(app, now, &mut |machine, raw, stop| {
                let (sent, reply) = mpsc::channel();
                self.asked.push((machine.to_string(), raw.to_string()));
                self.ends.push(sent);
                self.stops.push(stop);
                Some(reply)
            });
        }
    }

    fn first_text(app: &App) -> String {
        app.preview
            .as_ref()
            .map(|text| {
                text.lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_far_verb_is_asked_for_the_visible_screen_with_its_colours() {
        assert_eq!(
            read_args("%16"),
            ["pane", "read", "%16", "--source", "visible", "--ansi"]
        );
    }

    #[test]
    fn a_reply_is_the_text_of_the_last_line_that_is_an_envelope() {
        let line = br#"{"id":"cli:pane:read","result":{"pane_id":"s:1.%3","text":"hi\nthere"}}"#;
        assert_eq!(reply_text(line).as_deref(), Some("hi\nthere"));
        let mut said = b"welcome back\n".to_vec();
        said.extend_from_slice(line);
        said.extend_from_slice(b"\n\n");
        assert_eq!(reply_text(&said).as_deref(), Some("hi\nthere"));
        let missing = br#"{"id":"cli:pane:read","error":{"code":"not_found","message":"no"}}"#;
        assert_eq!(reply_text(missing), None);
        assert_eq!(reply_text(b"this is not json"), None);
        assert_eq!(reply_text(b""), None);
    }

    /// Text is data: a far pane's output holds a clipboard write, a title, cursor moves, a bell and a
    /// carriage return, and none of it may reach a span of the preview. The colour stays.
    #[test]
    fn no_span_of_a_shown_preview_holds_a_control_character() {
        let far = "\x1b[1;31mred\x1b[0m \x1b]52;c;aGk=\x07after\x1b]0;a title\x07\n\
                   \x1b[2J\x1b[10;5Hmoved\x07\rcarriage\x1b[?1049h\x1bP1;2|dcs-payload\x1b\\ok\u{202e}rtl\n";
        let text = shown(far);
        let mut red = false;
        for line in &text.lines {
            for span in &line.spans {
                assert!(
                    !span.content.chars().any(char::is_control),
                    "a control character in {:?}",
                    span.content
                );
                red |= span.style.fg == Some(Color::Red) && span.content.contains("red");
            }
        }
        let flat = first_text_of(&text);
        assert!(!flat.contains("]52") && !flat.contains("aGk="), "{flat:?}");
        assert!(
            !flat.contains("dcs-payload") && !flat.contains('\u{202e}'),
            "a device control string and a reordering character are taken out: {flat:?}"
        );
        assert!(red, "the colour is kept: {text:?}");
        assert!(flat.contains("after") && flat.contains("moved") && flat.contains("ok"));
    }

    fn first_text_of(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn a_reply_is_cut_before_it_is_looked_at() {
        let long = "x".repeat(MAX_REPLY * 3);
        let flat = first_text_of(&shown(&long));
        assert_eq!(flat.len(), MAX_REPLY);
        // On a character: a cut inside one would panic. Three bytes each, and 65536 is not a multiple of three,
        // so the byte at the cap is inside a character and the cut has to move back to the one before.
        let wide = "\u{20ac}".repeat(MAX_REPLY);
        let flat = first_text_of(&shown(&wide));
        assert!(flat.chars().all(|c| c == '\u{20ac}'), "{flat:?}");
        assert_eq!(flat.chars().count(), MAX_REPLY / 3);
    }

    #[test]
    fn at_most_one_read_is_in_flight_per_machine_and_one_machine_does_not_hold_up_another() {
        let cfg = Config::default();
        let mut app = app_over(
            &cfg,
            &[view("studio", true, &[8, 9]), view("pi", true, &[5])],
        );
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(1));
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(5));
        assert_eq!(
            reads.asked,
            [("studio".to_string(), "%8".to_string())],
            "a read that has not answered is the only one, however many ticks pass: the raw id is asked"
        );
        // pi is another machine: its read starts while studio's hangs.
        cursor_on(&mut app, "pi:%5");
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(6));
        assert_eq!(reads.asked.len(), 2);
        assert_eq!(reads.asked[1], ("pi".to_string(), "%5".to_string()));
    }

    #[test]
    fn a_reply_for_another_pane_is_dropped_and_the_new_panes_own_text_is_drawn() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8, 9])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        cursor_on(&mut app, "studio:%9");
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(500));
        assert_eq!(app.preview_shows(), Some("studio:%9"));
        assert!(
            app.preview.is_none(),
            "the old pane's text has no place under the new row"
        );
        // The old pane's read answers: nothing of it is drawn.
        reads.ends[0].send(Some("text of eight".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(1));
        assert!(app.preview.is_none(), "{}", first_text(&app));
        // The positive half: the read of nine, started now that the machine is free, is drawn.
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(1600));
        assert_eq!(reads.asked.len(), 2);
        assert_eq!(reads.asked[1].1, "%9");
        reads.ends[1].send(Some("text of nine".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(2));
        assert_eq!(first_text(&app), "text of nine");
        assert_eq!(app.preview_shows(), Some("studio:%9"));
    }

    #[test]
    fn a_failed_read_keeps_the_last_text_with_its_age_and_a_hung_one_lets_the_age_grow() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.ends[0].send(Some("first".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(100));
        assert_eq!(first_text(&app), "first");
        assert_eq!(
            app.preview_age,
            Some(Duration::from_millis(100)),
            "the age counts from when the read began"
        );
        // The next read fails (the far verb answered an error, a bad envelope, ssh failed).
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(1));
        assert_eq!(reads.asked.len(), 2);
        reads.ends[1].send(None).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(3));
        assert_eq!(first_text(&app), "first", "the last text stays");
        assert_eq!(
            app.preview_age,
            Some(Duration::from_secs(3)),
            "and its age is the time since the last capture that worked"
        );
        // A third read hangs: ticks go by, nothing answers, the age keeps growing.
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(4));
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(9));
        assert_eq!(reads.asked.len(), 3);
        assert_eq!(app.preview_age, Some(Duration::from_secs(9)));
        assert_eq!(first_text(&app), "first");
    }

    /// Kills the pid a test's process wrote to its file, if a failed assert left it running: only while armed
    /// (the test disarms it once it saw the pid gone, when the number may be someone else's), and only if the
    /// pid's command line still names the process the test started.
    struct KillPidIn {
        pidfile: std::path::PathBuf,
        far: &'static str,
        armed: bool,
    }

    impl KillPidIn {
        fn new(pidfile: std::path::PathBuf, far: &'static str) -> KillPidIn {
            KillPidIn {
                pidfile,
                far,
                armed: true,
            }
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for KillPidIn {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let Some(pid) = std::fs::read_to_string(&self.pidfile)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
                .filter(|pid| *pid > 1)
            else {
                return;
            };
            let command = Command::new("ps")
                .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            if command.contains(self.far) {
                // SAFETY: kill(2) takes two integers; the pid is the test's process, checked just now by its command.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the pid exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Waits (20 s) for `ready`, and panics with `what` when it never held.
    fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The real thing: a child run through [`launch_command`] is ended by its pid, and reaped, when the preview
    /// that holds its read is dropped, so no ssh outlives the app.
    #[test]
    fn dropping_the_preview_ends_the_process_of_a_read_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut guard = KillPidIn::new(pidfile.clone(), "sleep 30");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("echo $$ > '{}'; exec sleep 30", pidfile.display()),
        ]);
        let stop = Ender::new();
        // The thread looks every ten seconds: only Drop itself can end the child in time.
        let reply =
            launch_command_with_look(command, stop.clone(), Duration::from_secs(10)).unwrap();
        let mut remote = RemotePreview::default();
        remote.flights.insert(
            "studio".into(),
            Flight {
                pane: "studio:%8".into(),
                reply,
                stop: stop.clone(),
                began: Instant::now(),
            },
        );
        let mut pid = 0;
        wait_until("the process writing its pid", || {
            pid = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0);
            pid > 0
        });
        assert!(
            alive(pid),
            "positive half: the process runs before the drop"
        );
        drop(remote);
        assert!(stop.is_set(), "the drop sets the flag");
        // At once, with no wait: Drop kills and reaps the child itself. Mutants caught: Drop without
        // `stop.end()`, and Drop that only sets the flag (the thread would end the child ten seconds on).
        assert!(!alive(pid), "the process {pid} outlived the preview");
        guard.disarm();
    }

    /// A child of this test, ended when the test is over, a panic before its own clean-up included.
    struct Sleeper(std::process::Child);

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A child of this test that sleeps `secs` seconds, with its pid written to `pidfile`.
    fn sleeper(secs: &str, pidfile: &std::path::Path) -> Sleeper {
        let child = Command::new("sleep").arg(secs).spawn().unwrap();
        std::fs::write(pidfile, child.id().to_string()).unwrap();
        Sleeper(child)
    }

    /// The guard signals only a pid that is still the process the test recorded, and nothing once disarmed:
    /// a number that was reused by another process, or one the test saw gone, is left alone.
    #[test]
    fn the_pid_guard_signals_only_the_process_it_recorded_and_only_while_armed() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let ended = |child: &mut std::process::Child| {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if child.try_wait().unwrap().is_some() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        // A pid of another process (what a reused number is): armed, but the command line differs.
        let mut other = sleeper("293", &pidfile);
        drop(KillPidIn::new(pidfile.clone(), "sleep 291"));
        std::thread::sleep(Duration::from_millis(300));
        let other_kept = other.0.try_wait().unwrap().is_none();
        // The recorded process, disarmed: left alone.
        let mut mine = sleeper("291", &pidfile);
        let mut disarmed = KillPidIn::new(pidfile.clone(), "sleep 291");
        disarmed.disarm();
        drop(disarmed);
        std::thread::sleep(Duration::from_millis(300));
        let disarmed_kept = mine.0.try_wait().unwrap().is_none();
        // Positive half: armed, and still that process, it is ended.
        drop(KillPidIn::new(pidfile.clone(), "sleep 291"));
        let mine_ended = ended(&mut mine.0);
        assert!(
            other_kept,
            "a pid whose command is not the recorded one was signalled"
        );
        assert!(disarmed_kept, "a disarmed guard signalled");
        assert!(
            mine_ended,
            "positive half: an armed guard ends the process it recorded"
        );
    }

    #[test]
    fn a_pane_change_ends_the_old_read_and_the_new_panes_read_starts_once_the_old_one_answered() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8, 9])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(300));
        assert!(
            !reads.stops[0].is_set(),
            "positive half: a read of the cursor's own pane is left alone"
        );
        cursor_on(&mut app, "studio:%9");
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(600));
        assert!(reads.stops[0].is_set(), "the old read is told to end");
        assert_eq!(
            reads.asked.len(),
            1,
            "its slot is held until it has answered"
        );
        reads.ends[0].send(None).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(1200));
        assert_eq!(reads.asked.len(), 2);
        assert_eq!(reads.asked[1].1, "%9");
        assert!(!reads.stops[1].is_set());
    }

    #[test]
    fn a_cursor_that_leaves_every_machine_pane_ends_the_read_in_flight() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(300));
        assert!(!reads.stops[0].is_set(), "positive half");
        let mut moved = 0;
        while app.preview_remote_pane().is_some() {
            app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            moved += 1;
            assert!(moved < 80, "the cursor never left studio's panes");
        }
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(600));
        assert!(
            reads.stops[0].is_set(),
            "a cursor on this Mac's row ends the read"
        );
        assert_eq!(reads.asked.len(), 1, "and starts none");
    }

    #[test]
    fn coming_back_to_a_pane_shows_no_age_until_a_new_reply_and_then_counts_from_it() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.ends[0].send(Some("first".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(100));
        assert_eq!(first_text(&app), "first");
        assert!(app.preview_age.is_some(), "positive half: the age is told");
        while app.preview_remote_pane().is_some() {
            app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(1));
        // What the run loop does on this Mac's row: its own capture takes the panel.
        let local = app.preview_pane();
        app.set_preview(local, None);
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(60));
        assert!(app.preview.is_none());
        assert_eq!(app.preview_age, None, "no old age over an empty preview");
        reads.ends[1].send(Some("again".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_secs(61));
        assert_eq!(first_text(&app), "again");
        assert_eq!(
            app.preview_age,
            Some(Duration::from_secs(1)),
            "it counts from the read that gave the text"
        );
    }

    /// `READ_BOUND_SECS` of `tests/app_preview.rs` (the hang test's bound, which that file cannot read from here)
    /// is this number: a change of the bound fails this test until the other file follows.
    #[test]
    fn the_read_bound_is_the_one_the_preview_hang_test_names() {
        let test_file = include_str!("../../../tests/app_preview.rs");
        let named = test_file
            .lines()
            .find_map(|line| line.strip_prefix("const READ_BOUND_SECS: u64 = "))
            .and_then(|rest| rest.trim_end_matches(';').parse::<u64>().ok());
        // Whole durations, not whole seconds: a bound of 8.5 s would pass a comparison of seconds.
        assert_eq!(
            named.map(Duration::from_secs),
            Some(READ_BOUND),
            "READ_BOUND_SECS in tests/app_preview.rs must be READ_BOUND"
        );
    }

    #[test]
    fn the_age_counts_from_when_the_read_began_not_from_when_its_reply_was_taken() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.ends[0].send(Some("slow".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + TICK * 2);
        assert_eq!(first_text(&app), "slow");
        assert!(
            app.preview_age.is_some_and(|age| age >= TICK * 2),
            "{:?}",
            app.preview_age
        );
    }

    #[test]
    fn a_machine_that_is_away_or_a_preview_that_is_off_starts_no_read() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("pi", false, &[5])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "pi:%5");
        reads.step(&mut remote, &mut app, t0);
        assert!(reads.asked.is_empty(), "pi is not live");

        let mut app = app_over(&cfg, &[view("studio", true, &[8])]);
        app.show_preview = false;
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        assert!(reads.asked.is_empty(), "[ui] preview is off");
        app.show_preview = true;
        reads.step(&mut remote, &mut app, t0);
        assert_eq!(
            reads.asked.len(),
            1,
            "positive half: the same row reads once it is on"
        );
    }

    #[test]
    fn reads_are_started_at_most_once_in_half_a_tick() {
        let cfg = Config::default();
        let mut app = app_over(&cfg, &[view("studio", true, &[8])]);
        let (mut remote, mut reads) = (RemotePreview::default(), Reads::default());
        let t0 = Instant::now();
        cursor_on(&mut app, "studio:%8");
        reads.step(&mut remote, &mut app, t0);
        reads.ends[0].send(Some("a".into())).unwrap();
        reads.step(&mut remote, &mut app, t0 + Duration::from_millis(10));
        assert_eq!(
            reads.asked.len(),
            1,
            "the machine is free, but a read was just started"
        );
        reads.step(&mut remote, &mut app, t0 + TICK);
        assert_eq!(reads.asked.len(), 2);
    }
}
