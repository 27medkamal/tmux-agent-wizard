//! The paste-image key's work, off the screen.
//!
//! Reading the clipboard and sending the image can take long (a machine over ssh is given up on after thirty
//! seconds), so the popup and `taw app` start each paste as a process of its own, `taw image paste` (see
//! [`crate::image::paste_command`]), in a session of its own, and keep drawing and taking keys while it runs. A
//! thread waits for that process and its one line of answer becomes the toast, which comes back through
//! [`Pasting::poll`], which the screen's loop calls on each turn. One paste is under way at a time: a second
//! key says so and starts nothing.
//!
//! Leaving never waits: the screen goes and the process finishes on its own, within its own bounds
//! ([`crate::image::send_bound`] and [`crate::image::CLIPBOARD_BOUND`]), so nothing outlives those.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::paths::Paths;
use crate::tmux::Tmux;

/// What the key says while a paste is under way.
pub const UNDER_WAY: &str = "an image is being pasted already";

/// What the key says once the paste is started, for the pane `id` (the list's id).
pub fn started_words(id: &str) -> String {
    format!("pasting the image into {id}")
}

/// What a paste whose thread ended with no answer says.
const NO_ANSWER: &str = crate::image::NO_ANSWER;

/// The work of one paste for the list's pane id and the tmux client the screen is drawn on, if it has one: the
/// words the toast says when it ends.
type Job = Arc<dyn Fn(&Tmux, &Paths, &str, Option<&str>) -> String + Send + Sync>;

/// The paste that is under way, if one is.
pub struct Pasting {
    job: Job,
    /// The tmux client the screen is drawn on (the popup's), which the paste's process shows a late failure on.
    client: Option<String>,
    flight: Option<Receiver<String>>,
    /// The words of a paste that ended before the next look, kept for [`Pasting::poll`].
    ready: Option<String>,
}

impl Default for Pasting {
    /// A pasting whose process is this program's own `taw`.
    fn default() -> Pasting {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("taw"));
        Pasting::spawning(exe, Vec::new())
    }
}

impl Pasting {
    /// A pasting whose process is `taw_bin`'s `image paste`, run with `env` added to its environment (a test's
    /// way of pointing it at a stand-in ssh and clipboard).
    pub fn spawning(taw_bin: PathBuf, env: Vec<(String, String)>) -> Pasting {
        Pasting::with(Arc::new(move |tmux, paths, id, client| {
            let mut cmd = crate::image::paste_command(&taw_bin, tmux, paths, id, client);
            cmd.envs(env.iter().map(|(name, value)| (name, value)));
            crate::image::run_paste_child(cmd)
        }))
    }

    /// A pasting whose work is `job` (a test's stand-in for the process).
    pub fn with(job: Job) -> Pasting {
        Pasting {
            job,
            client: None,
            flight: None,
            ready: None,
        }
    }

    /// This pasting for a screen drawn on the tmux client `client` (none for a screen outside tmux).
    pub fn on_client(mut self, client: Option<&str>) -> Pasting {
        self.client = client.map(str::to_string);
        self
    }

    /// The paste-image key for the pane `id` names: `toast` is given the words of the key itself
    /// ([`Pasting::start`]), after the words of a paste that has just ended and not yet been polled when there are
    /// some (taken into the toast before the key is answered, so that a quick second key is not told a paste is
    /// under way, and so that the end of the first, a failure say, is not hidden by the start of the second).
    pub fn press(&mut self, tmux: &Tmux, paths: &Paths, id: &str, toast: impl FnOnce(String)) {
        let ended = self.poll();
        let started = self.start(tmux, paths, id);
        toast(match ended {
            Some(ended) => format!("{ended}; {started}"),
            None => started,
        });
    }

    /// Starts the paste of the clipboard's image into the pane `id` names, on a thread of its own, and gives the
    /// words to toast now: that it started, or that one is under way already (and nothing is started).
    pub fn start(&mut self, tmux: &Tmux, paths: &Paths, id: &str) -> String {
        self.look();
        if self.flight.is_some() {
            return UNDER_WAY.to_string();
        }
        // The words of an ended paste that nobody polled are dropped by a new one only through `press`.
        self.ready = None;
        let (tx, rx) = mpsc::channel();
        let (job, tmux, paths, pane, client) = (
            Arc::clone(&self.job),
            tmux.clone(),
            paths.clone(),
            id.to_string(),
            self.client.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("taw-image".to_string())
            .spawn(move || {
                let _ = tx.send(job(&tmux, &paths, &pane, client.as_deref()));
            });
        match spawned {
            Ok(_) => {
                self.flight = Some(rx);
                started_words(id)
            }
            Err(err) => format!("could not start the image paste: {err}"),
        }
    }

    /// The words of the paste that has ended since the last call, once.
    pub fn poll(&mut self) -> Option<String> {
        self.look();
        self.ready.take()
    }

    /// Whether a paste is under way (not yet taken by [`Pasting::poll`]).
    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.flight.is_some() || self.ready.is_some()
    }

    /// Whether a paste has ended and its words wait to be polled (the state a key can find a paste in).
    #[cfg(test)]
    fn has_ended(&mut self) -> bool {
        self.look();
        self.ready.is_some()
    }

    /// Moves the answer of an ended thread into `ready`.
    fn look(&mut self) {
        let Some(rx) = &self.flight else {
            return;
        };
        match rx.try_recv() {
            Ok(words) => {
                self.flight = None;
                self.ready = Some(words);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.flight = None;
                self.ready = Some(NO_ANSWER.to_string());
            }
        }
    }
}

/// A stand-in for the clipboard and the delivery that waits for `gate` to be sent a value (at most 10 s, so that
/// a broken run ends) and answers `said`: a test holds the paste under way with it.
#[cfg(test)]
pub(crate) fn gated(gate: Receiver<()>, said: &'static str) -> Job {
    let gate = std::sync::Mutex::new(gate);
    Arc::new(move |_, _, _, _| {
        let _ = gate
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10));
        said.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn rig() -> (Tmux, Paths) {
        let paths = Paths::rooted_at("/nonexistent/config".into(), "/nonexistent/state".into());
        let tmux = Tmux {
            bin: "/nonexistent/tmux".into(),
            socket: None,
        };
        (tmux, paths)
    }

    fn wait_for_words(pasting: &mut Pasting) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(words) = pasting.poll() {
                return words;
            }
            assert!(Instant::now() < deadline, "the paste never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The paste runs on a thread of its own: starting it returns at once while its work still waits, and
    /// nothing is said of the end until the work has ended.
    #[test]
    fn starting_a_paste_does_not_wait_for_its_work() {
        let (tmux, paths) = rig();
        let (open, gate) = mpsc::channel();
        let mut pasting = Pasting::with(gated(gate, "image pasted into %16"));
        let began = Instant::now();
        assert_eq!(pasting.start(&tmux, &paths, "%16"), started_words("%16"));
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "starting waited for the work: {:?}",
            began.elapsed()
        );
        assert_eq!(pasting.poll(), None, "the work has not ended");
        assert!(pasting.is_busy());
        open.send(()).unwrap();
        assert_eq!(wait_for_words(&mut pasting), "image pasted into %16");
        assert!(!pasting.is_busy());
    }

    /// A second key while one is under way says so and starts nothing; once the first has ended a new one starts.
    #[test]
    fn a_second_paste_while_one_is_under_way_says_so() {
        let (tmux, paths) = rig();
        let (open, gate) = mpsc::channel();
        let mut pasting = Pasting::with(gated(gate, "first"));
        pasting.start(&tmux, &paths, "%16");
        assert_eq!(pasting.start(&tmux, &paths, "%17"), UNDER_WAY);
        open.send(()).unwrap();
        assert_eq!(wait_for_words(&mut pasting), "first");
        assert_eq!(
            pasting.start(&tmux, &paths, "%17"),
            started_words("%17"),
            "an ended paste does not hold the next one"
        );
        assert_eq!(wait_for_words(&mut pasting), "first");
    }

    /// A paste that has just ended and not yet been polled is taken into its toast before the next key is
    /// answered: the key starts a new paste and is not told one is under way.
    #[test]
    fn a_quick_second_key_after_the_first_paste_ended_is_not_told_one_is_under_way() {
        let (tmux, paths) = rig();
        let (open, gate) = mpsc::channel();
        let mut pasting = Pasting::with(gated(gate, "image pasted into %16"));
        pasting.start(&tmux, &paths, "%16");
        open.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !pasting.has_ended() {
            assert!(Instant::now() < deadline, "the paste never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut said = Vec::new();
        pasting.press(&tmux, &paths, "%17", |words| said.push(words));
        assert_eq!(
            said,
            [format!("image pasted into %16; {}", started_words("%17"))],
            "the ended paste is toasted, joined to the start of the next"
        );
        assert!(!said.iter().any(|words| words.contains(UNDER_WAY)));
    }

    /// A paste that failed and a new key that arrive in the same frame share one toast, the failure first: the
    /// start of the new paste does not hide it. With no paste ended, the toast is the start alone.
    #[test]
    fn an_ended_pastes_error_and_a_new_key_in_one_frame_are_one_toast() {
        let (tmux, paths) = rig();
        let (open, gate) = mpsc::channel();
        let mut pasting = Pasting::with(gated(gate, "studio: ssh exited 255 without saying why"));
        pasting.start(&tmux, &paths, "studio:%16");
        open.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !pasting.has_ended() {
            assert!(Instant::now() < deadline, "the paste never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut said = Vec::new();
        pasting.press(&tmux, &paths, "%17", |words| said.push(words));
        assert_eq!(
            said,
            [format!(
                "studio: ssh exited 255 without saying why; {}",
                started_words("%17")
            )]
        );
        let mut again = Vec::new();
        let mut fresh = Pasting::with(gated(mpsc::channel().1, "x"));
        fresh.press(&tmux, &paths, "%18", |words| again.push(words));
        assert_eq!(again, [started_words("%18")]);
    }

    /// The pasting hands the screen's client to the paste's process, and none when the screen has none.
    #[test]
    fn the_screens_client_goes_to_the_paste() {
        let (tmux, paths) = rig();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job: Job = {
            let seen = Arc::clone(&seen);
            Arc::new(move |_, _, _, client| {
                seen.lock().unwrap().push(client.map(str::to_string));
                "done".to_string()
            })
        };
        let mut on = Pasting::with(Arc::clone(&job)).on_client(Some("/dev/ttys7"));
        on.start(&tmux, &paths, "%1");
        assert_eq!(wait_for_words(&mut on), "done");
        let mut off = Pasting::with(job);
        off.start(&tmux, &paths, "%1");
        assert_eq!(wait_for_words(&mut off), "done");
        assert_eq!(
            *seen.lock().unwrap(),
            [Some("/dev/ttys7".to_string()), None]
        );
    }
}
