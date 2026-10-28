//! What the app does with a toast, wherever the person is: the app's top line in the list, nothing inside the
//! machine the toast is about, a forwarded toast through the machine the person is inside otherwise (in this
//! process for this Mac), and, for the one app that holds the lease, the sound and the push. Also the toast
//! of a machine that drops off ([`Feed::notice_drops`]).
//!
//! The toasts of a machine come from its feed, with `lease` set by the far side for this app's holder. The
//! toasts of this Mac are lines its own watcher appended to the events file under a fresh lease
//! ([`crate::notify::announce_with`]), read by a tail that was made when the app started: what was in the
//! file before is never news.
//!
//! Only the keeper app hears another machine's toasts, because it holds the connections: a second app on this
//! Mac shows this Mac's toasts where its own person is, and never a machine's. A toast that arrives while a
//! lease lapses, or just after the keeper that held it died, can get neither the sound nor the push, for the
//! 15 s the lease takes to lapse (accepted).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{Feed, Record};
use crate::config::ToastDelivery;
use crate::machine::ssh::{self, Mode};
use crate::machine::wire::{Down, Toast};
use crate::machine::{Machine, files, this_machine_word, words};
use crate::notify::{self, Class, Forwarded, Notification, NotifiedAgent, SoundChoice};
use crate::paths::Paths;
use crate::tui::app::App;
use crate::tui::data::Snapshot;

/// How long a forward waits for the machine before its ssh is ended: a machine that cannot be reached loses
/// the toast, and nothing waits for it.
const FORWARD_BOUND: Duration = Duration::from_secs(8);

/// How many forwards run at once: sshd keeps ten sessions per connection (`MaxSessions`), and the preview
/// and the actions of the machine need room among them. A toast that comes while this many run is lost.
const FORWARDS_AT_ONCE: usize = 3;

/// The forwards in flight: how many, which ssh children they run, and whether the app is gone.
#[derive(Default)]
struct Flight {
    /// Forwards started and not ended, the ones that have not run their ssh yet included.
    running: usize,
    /// The pids of the ssh children that run now, each made by [`run_bounded`], which takes its pid out
    /// in the same step in which it reaps the child.
    pids: Vec<u32>,
    /// The toaster was dropped: a forward that runs its ssh after that ends it at once.
    closed: bool,
}

fn locked(flight: &Mutex<Flight>) -> MutexGuard<'_, Flight> {
    flight.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the app keeps for its toasts.
pub(super) struct Toaster {
    tail: files::EventTail,
    taw_bin: PathBuf,
    /// The words for the app's top line, shown by the next tick ([`Feed::say_toasts`]); the newest wins.
    top: Option<String>,
    /// Whether each machine was live the last time it was looked at, in the config's order.
    live: Vec<bool>,
    /// The machines that listed a session the last time the list was drawn, by name: one of them that lists
    /// none now (live, with its state in) has just lost its last session.
    had_sessions: std::collections::HashSet<String>,
    /// The forwards in flight, ended with the app.
    forwards: Arc<Mutex<Flight>>,
    /// How long a forward waits for its machine.
    bound: Duration,
}

impl Toaster {
    /// The tail starts at the end of this Mac's events file.
    pub(super) fn new(paths: &Paths, taw_bin: &Path) -> Toaster {
        Toaster {
            tail: files::EventTail::at_end(paths),
            taw_bin: taw_bin.to_path_buf(),
            top: None,
            live: Vec::new(),
            had_sessions: Default::default(),
            forwards: Arc::default(),
            bound: FORWARD_BOUND,
        }
    }

    /// The machines are all unconnected again (the connections were started anew), so none of them was live
    /// at the last look: one that was is not said to have dropped off.
    pub(super) fn forget_live(&mut self) {
        self.live.clear();
    }

    /// The list of machines changed: what is kept by a machine's place moves to its new place (`now_of` says
    /// where each machine of the old list went, `None` for one that is gone), and nothing stays of a machine that
    /// is no longer listed.
    pub(super) fn machines_changed(&mut self, now_of: &[Option<usize>], machines: &[Machine]) {
        let mut live = vec![false; machines.len()];
        for (old, now) in now_of.iter().enumerate() {
            if let Some(now) = now {
                live[*now] = self.live.get(old).copied().unwrap_or(false);
            }
        }
        self.live = live;
        self.had_sessions
            .retain(|name| machines.iter().any(|m| &m.name == name));
    }

    /// The words for each machine of `snap` that listed a session when this was last asked and lists none now
    /// ([`words::no_session_now`]): its last session ended, and its list has the row that starts another. A machine
    /// that is not live or has said nothing is not told of: it may only be out of sight.
    pub(super) fn no_session_news(&mut self, snap: &Snapshot) -> Vec<String> {
        no_session_news(&mut self.had_sessions, snap)
    }

    /// What was waiting for the top line is not news after an attach.
    pub(super) fn forget_top(&mut self) {
        self.top = None;
    }
}

impl Drop for Toaster {
    /// The forwards still running are ended by their pids, which only this module made: the app does not
    /// leave an ssh behind. A pid that was reaped has been taken out of the list in the step that reaped it,
    /// under the same lock, so one that is gone is never signalled.
    fn drop(&mut self) {
        let mut flight = locked(&self.forwards);
        flight.closed = true;
        for pid in &flight.pids {
            // SAFETY: the pid of an ssh child of this module that has not been reaped yet (see `Flight`).
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// Where the person is.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) enum Place {
    /// In the app's list.
    List,
    /// Inside this Mac's own tmux.
    Mac,
    /// Inside the machine of `[machines]` called so.
    Machine(String),
}

/// What a toast is about.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) enum About {
    Mac,
    Machine(String),
}

impl About {
    /// The word the toast's place line and the top line call it by.
    fn word(&self) -> &str {
        match self {
            About::Mac => this_machine_word(),
            About::Machine(name) => name,
        }
    }

    /// `TAW_MACHINE` of the push: empty for this Mac.
    fn hook_word(&self) -> &str {
        match self {
            About::Mac => "",
            About::Machine(name) => name,
        }
    }
}

/// What is done with one toast.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan {
    /// Its words go on the app's top line.
    pub(super) top: bool,
    /// It is drawn by this process on this Mac's clients.
    pub(super) in_process: bool,
    /// It is forwarded through this machine.
    pub(super) forward_through: Option<String>,
    /// This app makes the sound and the push: it holds the lease.
    pub(super) speak: bool,
}

/// What to do with a toast `about` something, for a person at `place`. The show follows this Mac's `[ui.toast]
/// delivery` (`off` shows none); the sound and the push do not, and are the lease holder's alone.
///
/// Under `system` a toast about a machine is posted on this Mac's desktop by this process wherever the person
/// is (the machine it is about included), and is never forwarded: another machine's desktop is nobody's. In
/// the list it also goes to the top line. This Mac's own toast was posted by its watcher. `terminal` and
/// `herdr` draw where the person is: the top line in the list, this process inside this Mac, and a forward
/// through the machine they are inside (drawn there under this Mac's delivery, which the forward carries).
pub(super) fn plan(place: &Place, about: &About, delivery: ToastDelivery, lease: bool) -> Plan {
    let mut plan = Plan {
        top: false,
        in_process: false,
        forward_through: None,
        speak: lease,
    };
    if delivery == ToastDelivery::Off {
        return plan;
    }
    if delivery == ToastDelivery::System {
        plan.top = *place == Place::List;
        plan.in_process = matches!(about, About::Machine(_));
        return plan;
    }
    match (place, about) {
        (Place::List, _) => plan.top = true,
        // This Mac showed its own toast where it was made.
        (Place::Mac, About::Mac) => {}
        (Place::Mac, About::Machine(_)) => plan.in_process = true,
        // The machine the person is inside showed its own.
        (Place::Machine(inside), About::Machine(name)) if inside == name => {}
        (Place::Machine(inside), _) => plan.forward_through = Some(inside.clone()),
    }
    plan
}

/// [`Toaster::no_session_news`] over `had`, the names of the machines that listed a session before.
fn no_session_news(had: &mut std::collections::HashSet<String>, snap: &Snapshot) -> Vec<String> {
    let mut news = Vec::new();
    for (at, header) in snap.machines.iter().enumerate() {
        if snap.spaces.iter().any(|space| space.machine == Some(at)) {
            had.insert(header.name.clone());
        } else if header.live && header.stated && !header.this_server && had.remove(&header.name) {
            news.push(words::no_session_now(&header.name));
        }
    }
    news
}

/// The machines that were live and are not now, by their place in the config; `was` is brought up to `now`.
/// A machine that was never live is not among them, and one that has dropped is not again until it has been
/// live again.
fn dropped(was: &mut Vec<bool>, now: &[bool]) -> Vec<usize> {
    was.resize(now.len(), false);
    let mut gone = Vec::new();
    for (at, (before, live)) in was.iter_mut().zip(now).enumerate() {
        if *before && !live {
            gone.push(at);
        }
        *before = *live;
    }
    gone
}

/// The words a forwarded toast is sent as: the hidden flags of `taw notification show`, each value joined to
/// its flag (a value that starts with `-` is still its flag's), the title after `--`. A toast about no agent
/// has no agent flags, and the toast of a machine dropping off says so by `--dropped`. `delivery` is this
/// Mac's `[ui.toast] delivery`, which the far taw draws the toast under: [`plan`] forwards only under `herdr`
/// and `terminal`, so those are the two words; under `off` or `system` no toast is forwarded and no flag is sent.
fn forward_words(f: &Forwarded, delivery: ToastDelivery) -> Vec<String> {
    let mut words = vec![
        "notification".to_string(),
        "show".to_string(),
        format!("--forwarded={}", f.machine),
    ];
    match delivery {
        ToastDelivery::Herdr => words.push("--delivery=herdr".to_string()),
        ToastDelivery::Terminal => words.push("--delivery=terminal".to_string()),
        ToastDelivery::Off | ToastDelivery::System => {}
    }
    if !f.agent.is_empty() {
        words.push(format!("--agent={}", f.agent));
        words.push(format!("--location={}", f.location));
        words.push(
            match f.class {
                Class::Request => "--kind=request",
                Class::Done => "--kind=done",
            }
            .to_string(),
        );
    }
    if f.dropped {
        words.push("--dropped".to_string());
    }
    if !f.body.is_empty() {
        words.push(format!("--body={}", f.body));
    }
    words.push("--".to_string());
    words.push(f.title.clone());
    words
}

/// The toast as another machine's agent's: what it said, as the toast of `word` is shown here.
fn forwarded(word: &str, toast: &Toast) -> Forwarded {
    Forwarded {
        machine: word.to_string(),
        agent: toast.agent.clone(),
        location: toast.location.clone(),
        class: class_of(&toast.kind),
        title: toast.title.clone(),
        body: toast.body.clone(),
        dropped: false,
    }
}

fn class_of(kind: &str) -> Class {
    if kind == "request" {
        Class::Request
    } else {
        Class::Done
    }
}

/// What a toast of this machine calls it where another machine draws it: `host`, this machine's short host
/// name (tmux's `#{host_short}`), where this machine's own word would name that machine. A host name that
/// cannot be read leaves this machine's word.
fn far_word(host: &str) -> String {
    if host.is_empty() {
        this_machine_word().to_string()
    } else {
        host.to_string()
    }
}

/// The words on the app's top line: the line the machine's watcher showed, with the machine ahead of it.
fn top_line(word: &str, toast: &Toast) -> String {
    let text = if toast.text.is_empty() {
        &toast.title
    } else {
        &toast.text
    };
    if word.is_empty() {
        return text.clone();
    }
    format!("{word}: {text}")
}

/// The notification the sound and the push are told of. The pane is the toast's own (that machine's, or this
/// Mac's), never looked up as a pane of another machine.
fn notification_of(toast: &Toast) -> Notification {
    let class = class_of(&toast.kind);
    Notification {
        class,
        title: toast.title.clone(),
        body: toast.body.clone(),
        pane_id: Some(toast.pane.clone()).filter(|pane| !pane.is_empty()),
        sound: match class {
            Class::Request => SoundChoice::Request,
            Class::Done => SoundChoice::Done,
        },
        agent: NotifiedAgent {
            label: toast.agent.clone(),
            kind: toast.agent_kind.clone(),
        },
    }
}

/// What the sound is told of the toast `n` is about `about`: for another machine's toast the pane is left
/// out, because its id is that machine's and the sound would read the pane of this Mac that has the same id
/// (the toast's own `agent_kind` is already in `n.agent.kind`). This Mac's toast keeps its pane.
fn sound_notification(about: &About, n: &Notification) -> Notification {
    Notification {
        pane_id: match about {
            About::Mac => n.pane_id.clone(),
            About::Machine(_) => None,
        },
        ..n.clone()
    }
}

impl Feed<'_> {
    /// Where the person is: the list, a client of this Mac, or a client of a machine (the record names it).
    fn place(&self) -> Place {
        place_of(&self.record)
    }

    /// Takes the toasts the machines announced and the ones this Mac's watcher left for the app, and does with
    /// each what [`plan`] says; then looks at which machines dropped off. What is taken is gone either way, so
    /// that it does not pile up.
    pub(super) fn announce_toasts(&mut self) {
        let heard = self.hub.toasts();
        let mine: Vec<Toast> = self
            .toasts
            .tail
            .read_new()
            .into_iter()
            .filter_map(|line| match line {
                Down::Toast(toast) => Some(toast),
                _ => None,
            })
            .collect();
        let place = self.place();
        for (at, toast) in heard {
            let Some(machine) = self.machines.get(at) else {
                continue;
            };
            let about = About::Machine(machine.name.clone());
            let lease = toast.lease;
            self.announce_toast(&place, &about, &toast, lease);
        }
        if !mine.is_empty() {
            // Whether this app holds this Mac's lease: the one that does speaks for every toast.
            let lease = self.hub.offer_lease();
            for toast in mine {
                self.announce_toast(&place, &About::Mac, &toast, lease);
            }
        }
        self.notice_drops(&place);
    }

    fn announce_toast(&mut self, place: &Place, about: &About, toast: &Toast, lease: bool) {
        let plan = plan(place, about, self.cfg.ui.toast.delivery, lease);
        let speak = plan.speak;
        // With no other machine listed nothing is told apart from this one, so no word names it.
        let word = match about {
            About::Mac if !self.lists_other_machines() => "",
            _ => about.word(),
        };
        // Drawn on another machine's status line, "this machine" would name that machine: the host name does.
        let drawn_word = match about {
            About::Mac if !word.is_empty() => far_word(&crate::machine::feed::host_name()),
            _ => word.to_string(),
        };
        self.show_where(plan, top_line(word, toast), forwarded(&drawn_word, toast));
        if speak {
            self.speak(about, toast);
        }
    }

    /// Whether a machine other than this Mac's own tmux server is listed in `[machines]`: the one case in which a
    /// toast of this Mac says so ([`Snapshot::has_other_machines`], from what the app holds).
    fn lists_other_machines(&self) -> bool {
        let views = self.hub.views();
        (0..self.machines.len()).any(|at| !views.get(at).is_some_and(|view| view.this_server))
    }

    /// Shows a toast where `plan` says.
    fn show_where(&mut self, plan: Plan, top: String, toast: Forwarded) {
        if plan.top {
            self.toasts.top = Some(top);
        }
        if plan.in_process
            && !self.desktop_stood_in(&toast)
            && let Err(err) = notify::show_forwarded(self.tmux, self.cfg, &toast)
        {
            log::warn!(
                "a toast of {} could not be drawn here: {err:#}",
                toast.machine
            );
        }
        if let Some(through) = plan.forward_through {
            self.forward(&through, &toast);
        }
    }

    /// Whether the desktop's post of `toast` is only recorded: in a test, under `system`, the notifier is
    /// never run. What is recorded is the title the desktop would show, the machine ahead of it except for a
    /// drop toast ([`notify::forwarded_parts`]), so a test sees a wrong `dropped` flag.
    #[cfg(test)]
    fn desktop_stood_in(&self, toast: &Forwarded) -> bool {
        self.cfg.ui.toast.delivery == ToastDelivery::System
            && seam(|seen| seen.desktop.push(notify::forwarded_parts(toast).0.title))
    }

    #[cfg(not(test))]
    fn desktop_stood_in(&self, _toast: &Forwarded) -> bool {
        false
    }

    /// The sound, once, with this Mac's `[ui.sound]`, and the push, once: `[hooks] notification` told that
    /// the toast is about `about` (`TAW_MACHINE` empty for this Mac).
    fn speak(&self, about: &About, toast: &Toast) {
        let n = notification_of(toast);
        let sound = sound_notification(about, &n);
        let machine = Some(about.hook_word());
        #[cfg(test)]
        if seam(|seen| {
            seen.sounds
                .push((sound.class, sound.title.clone(), sound.pane_id.clone()));
            seen.pushes
                .push((machine.map(str::to_string), n.pane_id.clone()));
        }) {
            return;
        }
        notify::play_sound(self.tmux, self.cfg, self.paths, &sound);
        let ctx = crate::plugin::PluginCtx {
            tmux: self.tmux,
            cfg: self.cfg,
            paths: self.paths,
            taw_bin: &self.toasts.taw_bin,
            server_env: None,
        };
        crate::hooks::notification_from(&ctx, &n, machine);
    }

    /// Sends the toast through the machine the person is inside, to be drawn there: the hidden flags of
    /// `taw notification show`, over the one ssh command every far verb goes through. It runs on its own
    /// thread, bounded, and its result is ignored: a machine that cannot be reached, or that does not know
    /// the flags, loses the toast and breaks nothing.
    fn forward(&self, through: &str, toast: &Forwarded) {
        let Some(machine) = self.machines.iter().find(|m| m.name == through) else {
            return;
        };
        let words = forward_words(toast, self.cfg.ui.toast.delivery);
        let args: Vec<&str> = words.iter().map(String::as_str).collect();
        let mut command = ssh::command(&self.ssh, self.paths, machine, Mode::Command, &args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        send(command, self.toasts.bound, &self.toasts.forwards);
    }

    /// Says `machine dropped off` once for each machine that was live and is not: where the person is, as a
    /// toast is, and never for the machine the person is inside (its attach ends by itself). Only the app that
    /// keeps the connections knows who is live.
    fn notice_drops(&mut self, place: &Place) {
        let Some(now) = self.live_flags() else {
            return;
        };
        for at in dropped(&mut self.toasts.live, &now) {
            let Some(machine) = self.machines.get(at) else {
                continue;
            };
            let name = machine.name.clone();
            // Never for the machine the person is inside: its attach ends by itself.
            if matches!(place, Place::Machine(inside) if *inside == name) {
                continue;
            }
            let words = words::dropped_toast(&name);
            let plan = plan(
                place,
                &About::Machine(name.clone()),
                self.cfg.ui.toast.delivery,
                false,
            );
            let toast = Forwarded {
                machine: name,
                agent: String::new(),
                location: String::new(),
                class: Class::Done,
                title: words.clone(),
                body: String::new(),
                dropped: true,
            };
            self.show_where(plan, words, toast);
        }
    }

    fn live_flags(&self) -> Option<Vec<bool>> {
        #[cfg(test)]
        if let Some(flags) = SEEN.with(|seen| seen.borrow().as_ref().and_then(|s| s.live.clone())) {
            return Some(flags);
        }
        self.hub.live_flags()
    }

    /// Puts the newest toast that waits on the app's top line.
    pub(super) fn say_toasts(&mut self, app: &mut App) {
        if let Some(words) = self.toasts.top.take() {
            app.toast(words);
        }
    }
}

fn place_of(record: &Record) -> Place {
    if record.record.client_pid == 0 {
        Place::List
    } else if record.record.inside.is_empty() {
        Place::Mac
    } else {
        Place::Machine(record.record.inside.clone())
    }
}

/// Runs `command` on a thread of its own and ends it when `bound` has passed. Nothing is read from it and
/// nothing is said of how it ended. With [`FORWARDS_AT_ONCE`] already running the toast is lost, and said so
/// in the log.
fn send(command: Command, bound: Duration, forwards: &Arc<Mutex<Flight>>) {
    #[cfg(test)]
    if seam(|seen| {
        let mut argv = vec![command.get_program().to_string_lossy().into_owned()];
        argv.extend(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned()),
        );
        seen.forwards.push(argv);
    }) {
        return;
    }
    {
        let mut flight = locked(forwards);
        if flight.running >= FORWARDS_AT_ONCE {
            log::warn!("a toast was not forwarded: {FORWARDS_AT_ONCE} forwards are running");
            return;
        }
        flight.running += 1;
    }
    let forwards = Arc::clone(forwards);
    std::thread::spawn(move || run_bounded(command, bound, &forwards));
}

fn run_bounded(mut command: Command, bound: Duration, forwards: &Mutex<Flight>) {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            locked(forwards).running -= 1;
            return;
        }
    };
    let pid = child.id();
    {
        let mut flight = locked(forwards);
        if flight.closed {
            // The app ended while the ssh was being started.
            let _ = child.kill();
            let _ = child.wait();
            flight.running -= 1;
            return;
        }
        flight.pids.push(pid);
    }
    let until = Instant::now() + bound;
    loop {
        {
            // Looking, reaping and taking the pid out are one step, and so is ending it: the pid is never
            // signalled once it is reaped.
            let mut flight = locked(forwards);
            let state = child.try_wait();
            if !matches!(state, Ok(None)) || Instant::now() >= until {
                if !matches!(state, Ok(Some(_))) {
                    // By its pid, which is this thread's own child: nothing else is touched.
                    let _ = child.kill();
                    let _ = child.wait();
                }
                flight.pids.retain(|p| *p != pid);
                flight.running -= 1;
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What a test's thread saw of what would have left the process, and what it set for the app to find.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(super) struct Seen {
    /// The ssh commands that would have been run, program first.
    pub(super) forwards: Vec<Vec<String>>,
    /// The sounds that would have played: the class, the title and the pane the sound was told of.
    pub(super) sounds: Vec<(Class, String, Option<String>)>,
    /// The pushes that would have run: the machine word given and the pane.
    pub(super) pushes: Vec<(Option<String>, Option<String>)>,
    /// The titles of the toasts that would have been posted on the desktop (under `system`).
    pub(super) desktop: Vec<String>,
    /// Which machines are live, in place of the connections' own answer.
    pub(super) live: Option<Vec<bool>>,
}

#[cfg(test)]
thread_local! {
    static SEEN: std::cell::RefCell<Option<Seen>> = const { std::cell::RefCell::new(None) };
}

/// Hands `note` what this test's thread records, when it records; `true` when it does, and then nothing
/// really leaves the process. The test thread's, as `Machines::announce_on_poll` is: not in the product build.
#[cfg(test)]
fn seam(note: impl FnOnce(&mut Seen)) -> bool {
    SEEN.with(|seen| match seen.borrow_mut().as_mut() {
        Some(seen) => {
            note(seen);
            true
        }
        None => false,
    })
}

/// Starts the recording on this thread and ends it when dropped, a failing assert included.
#[cfg(test)]
pub(super) struct Recording;

#[cfg(test)]
impl Recording {
    pub(super) fn start() -> Recording {
        SEEN.with(|seen| *seen.borrow_mut() = Some(Seen::default()));
        Recording
    }

    pub(super) fn seen(&self) -> Seen {
        SEEN.with(|seen| seen.borrow().clone().unwrap_or_default())
    }

    /// From now on the machines are as live as `flags` say.
    pub(super) fn live(&self, flags: &[bool]) {
        SEEN.with(|seen| {
            if let Some(seen) = seen.borrow_mut().as_mut() {
                seen.live = Some(flags.to_vec());
            }
        });
    }
}

#[cfg(test)]
impl Drop for Recording {
    fn drop(&mut self) {
        SEEN.with(|seen| *seen.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::{Stub, calls, stub_tmux};
    use crate::config::Config;
    use crate::machine::wire::Left;
    use crate::machine::{Machine, conn};
    use crate::tui::app_screen::tests::{feed_over, paths_in};
    use crate::tui::data::Snapshot;

    fn machine(name: &str, taw: Option<&str>) -> Machine {
        Machine {
            name: name.into(),
            ssh: name.into(),
            taw: taw.map(str::to_string),
        }
    }

    fn toast_of(agent: &str, body: &str) -> Toast {
        Toast {
            kind: "request".into(),
            pane: "%31".into(),
            text: format!("{agent} blocked \u{b7} garden:1 \u{b7} {body}"),
            title: format!("{agent} blocked \u{b7} garden:1"),
            body: body.into(),
            agent: agent.into(),
            agent_kind: agent.into(),
            location: "garden:1".into(),
            lease: false,
        }
    }

    /// What the recorder saw: the ssh commands' last words (the line the machine runs).
    fn lines_run(seen: &Seen) -> Vec<String> {
        seen.forwards
            .iter()
            .map(|argv| argv.last().cloned().unwrap_or_default())
            .collect()
    }

    fn top_of(feed: &mut Feed, cfg: &Config) -> Option<String> {
        let theme = crate::tui::theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(cfg, theme, String::new(), Snapshot::default());
        feed.say_toasts(&mut app);
        app.toast.as_ref().map(|(text, _)| text.clone())
    }

    fn drawn(log: &Path) -> Vec<String> {
        calls(log)
            .into_iter()
            .filter(|call| call[0] == "display-message" && call.get(1).is_some_and(|f| f == "-c"))
            .map(|call| call[3].clone())
            .collect()
    }

    const DEFAULT: ToastDelivery = ToastDelivery::Herdr;

    /// Where a toast goes for each place and subject: the top line in the list, nothing inside the machine it
    /// is about (and nothing for this Mac's own inside this Mac), this process on this Mac for a machine's,
    /// and a forward through the machine the person is inside otherwise. The sound and the push are the
    /// lease holder's, whatever `delivery` is.
    #[test]
    fn a_toast_goes_where_the_person_is() {
        let studio = || About::Machine("studio".into());
        let pi = || About::Machine("pi".into());
        let inside = |name: &str| Place::Machine(name.into());
        let go =
            |place: &Place, about: &About, delivery, lease| plan(place, about, delivery, lease);
        let nothing = |speak| Plan {
            top: false,
            in_process: false,
            forward_through: None,
            speak,
        };

        assert_eq!(
            go(&Place::List, &studio(), DEFAULT, false),
            Plan {
                top: true,
                ..nothing(false)
            }
        );
        assert_eq!(
            go(&Place::List, &About::Mac, DEFAULT, true),
            Plan {
                top: true,
                ..nothing(true)
            }
        );
        assert_eq!(
            go(&Place::Mac, &studio(), DEFAULT, true),
            Plan {
                in_process: true,
                ..nothing(true)
            }
        );
        assert_eq!(go(&Place::Mac, &About::Mac, DEFAULT, false), nothing(false));
        assert_eq!(
            go(&inside("studio"), &pi(), DEFAULT, false),
            Plan {
                forward_through: Some("studio".into()),
                ..nothing(false)
            }
        );
        assert_eq!(
            go(&inside("studio"), &About::Mac, DEFAULT, false),
            Plan {
                forward_through: Some("studio".into()),
                ..nothing(false)
            }
        );
        // Inside the machine the toast is about: it showed its own. The lease holder still speaks.
        assert_eq!(
            go(&inside("studio"), &studio(), DEFAULT, true),
            nothing(true)
        );
        // `system` posts a machine's toast on this Mac's desktop from this process wherever the person is,
        // forwards none, and adds the top line in the list; this Mac's own toast was posted by its watcher.
        let system = ToastDelivery::System;
        let desktop = |speak| Plan {
            in_process: true,
            ..nothing(speak)
        };
        assert_eq!(
            go(&Place::List, &studio(), system, false),
            Plan {
                top: true,
                ..desktop(false)
            }
        );
        assert_eq!(go(&Place::Mac, &studio(), system, true), desktop(true));
        assert_eq!(
            go(&inside("studio"), &studio(), system, false),
            desktop(false)
        );
        assert_eq!(go(&inside("pi"), &studio(), system, true), desktop(true));
        assert_eq!(
            go(&Place::List, &About::Mac, system, false),
            Plan {
                top: true,
                ..nothing(false)
            }
        );
        assert_eq!(go(&Place::Mac, &About::Mac, system, true), nothing(true));
        assert_eq!(
            go(&inside("studio"), &About::Mac, system, false),
            nothing(false)
        );
        // `off` shows none, anywhere, and does not mute the sound or the push.
        for place in [Place::List, Place::Mac, inside("studio")] {
            assert_eq!(
                go(&place, &pi(), ToastDelivery::Off, true),
                nothing(true),
                "{place:?}"
            );
        }
    }

    /// The sound is told of no pane for another machine's toast (its pane id is that machine's), and of the
    /// toast's own pane for this Mac's.
    #[test]
    fn the_sound_of_a_machines_toast_is_told_of_no_pane() {
        let toast = toast_of("claude", "hi");
        let n = notification_of(&toast);
        assert_eq!(n.pane_id.as_deref(), Some("%31"));
        let machine = sound_notification(&About::Machine("studio".into()), &n);
        assert_eq!(machine.pane_id, None);
        assert_eq!(machine.agent.kind, "claude", "the toast's own kind is kept");
        assert_eq!(machine.title, n.title);
        let mine = sound_notification(&About::Mac, &n);
        assert_eq!(mine.pane_id.as_deref(), Some("%31"));
    }

    /// Through `speak`, as the seam records it: a toast about studio naming `%31` is spoken with no pane, this
    /// Mac's own toast with its pane; the push keeps the pane for both.
    #[test]
    fn a_machines_toast_is_spoken_with_no_pane_and_this_macs_with_its_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let feed = feed_over(&tmux, &cfg, &paths);
        let recording = Recording::start();

        feed.speak(&About::Machine("studio".into()), &toast_of("claude", "hi"));
        feed.speak(&About::Mac, &toast_of("claude", "hi"));

        let seen = recording.seen();
        assert_eq!(
            seen.sounds.iter().map(|s| s.2.clone()).collect::<Vec<_>>(),
            vec![None, Some("%31".to_string())]
        );
        assert_eq!(
            seen.pushes,
            vec![
                (Some("studio".to_string()), Some("%31".to_string())),
                (Some(String::new()), Some("%31".to_string()))
            ]
        );
    }

    /// No tmux call about the pane `%31` is made for a machine's toast (the id is that machine's), and one is
    /// made for this Mac's toast, whose pane it is: the real `speak`, no seam, the sound off and a stub tmux.
    #[test]
    fn a_machines_toast_makes_no_tmux_call_about_a_pane_of_this_mac() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let mut cfg = Config::default();
        cfg.ui.sound.enabled = false;
        let paths = paths_in(dir.path());
        let feed = feed_over(&tmux, &cfg, &paths);
        let about_pane = |log: &Path| {
            calls(log)
                .into_iter()
                .filter(|call| call.iter().any(|word| word == "%31"))
                .count()
        };

        feed.speak(&About::Mac, &toast_of("claude", "hi"));
        assert!(about_pane(&log) > 0, "positive: this Mac's pane is read");
        let before = about_pane(&log);
        feed.speak(&About::Machine("studio".into()), &toast_of("claude", "hi"));

        assert_eq!(about_pane(&log), before, "no call about %31 for studio");
    }

    /// When this Mac's server comes up the connections are started again and are not connected yet: a machine
    /// that was live is not said to have dropped off (the toast of the same words is seen once for a drop that
    /// is real, after it has been live again).
    #[test]
    fn a_machine_that_was_live_is_not_said_to_drop_when_the_connections_start_again() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        // The stand-in knows its server's word (`#{pid} #{start_time}`), as a running server does.
        let script = dir.path().join("tmux");
        let text = std::fs::read_to_string(&script).unwrap();
        let text = text.replace(
            "      '#{pane_id}') echo '%5' ;;",
            "      '#{pane_id}') echo '%5' ;;\n      '#{pid} #{start_time}') echo '4211 1790000000' ;;",
        );
        std::fs::write(&script, text).unwrap();
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        // Another keeper holds the lock, so the hub made later follows and starts no ssh.
        let _keeper = files::take_keeper(&paths).expect("the keeper lock");
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None)];
        let recording = Recording::start();
        let words = "studio dropped off (last seen just now)";

        recording.live(&[true]);
        feed.announce_toasts();
        assert!(feed.server.is_empty(), "no server word yet");
        feed.follow_the_server(&tmux);
        assert_eq!(feed.server, "4211 1790000000", "the hub was replaced");
        recording.live(&[false]);
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg), None, "not live yet is not dropped");

        // Positive: live again, then really gone, it is said once.
        recording.live(&[true]);
        feed.announce_toasts();
        recording.live(&[false]);
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg).as_deref(), Some(words));
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg), None, "once");
    }

    /// Under `system` a machine's toast is posted on this Mac's desktop by this process wherever the person is,
    /// the machine it is about included, and is never forwarded; in the list it is on the top line too. The
    /// drop toast does the same, except for the machine the person is inside. The notifier is stood in for.
    #[test]
    fn under_system_a_machines_toast_is_posted_on_this_macs_desktop_and_forwarded_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let mut cfg = Config::default();
        cfg.ui.toast.delivery = ToastDelivery::System;
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        let recording = Recording::start();
        let toast = toast_of("claude", "hi");
        let title = toast.title.clone();

        // In the list: the desktop and the top line.
        feed.hub.announce(0, toast.clone(), Left::default());
        feed.announce_toasts();
        assert_eq!(recording.seen().desktop, vec![format!("studio: {title}")]);
        assert!(top_of(&mut feed, &cfg).is_some(), "and the top line");

        // Inside pi: still the desktop of this Mac, and nothing forwarded through pi.
        feed.record.attached(4242, "tty", "pi");
        feed.hub.announce(0, toast.clone(), Left::default());
        feed.announce_toasts();
        assert_eq!(recording.seen().desktop.len(), 2, "posted here");
        // Inside studio, the machine it is about: posted here too.
        feed.record.attached(4242, "tty", "studio");
        feed.hub.announce(0, toast, Left::default());
        feed.announce_toasts();
        let seen = recording.seen();
        assert_eq!(seen.desktop.len(), 3, "posted here: {seen:?}");
        assert!(seen.forwards.is_empty(), "forwarded through nobody");

        // The drop toast: posted for a machine the person is not inside, never for the one they are inside.
        recording.live(&[true, true]);
        feed.announce_toasts();
        recording.live(&[false, true]);
        feed.announce_toasts();
        assert_eq!(
            recording.seen().desktop.len(),
            3,
            "not for studio inside studio"
        );
        feed.record.attached(4242, "tty", "pi");
        recording.live(&[true, true]);
        feed.announce_toasts();
        recording.live(&[false, true]);
        feed.announce_toasts();
        let seen = recording.seen();
        assert_eq!(
            seen.desktop.last().map(String::as_str),
            Some("studio dropped off (last seen just now)")
        );
        assert!(seen.forwards.is_empty(), "forwarded through nobody");
    }

    /// A machine goes once: while it was never live, once it was and is not, and not again until it has been
    /// live again.
    #[test]
    fn a_machine_is_dropped_once_each_time_it_was_live_and_is_not() {
        let mut was = Vec::new();
        assert_eq!(dropped(&mut was, &[false, false]), Vec::<usize>::new());
        assert_eq!(dropped(&mut was, &[true, false]), Vec::<usize>::new());
        assert_eq!(dropped(&mut was, &[false, false]), vec![0]);
        assert_eq!(dropped(&mut was, &[false, false]), Vec::<usize>::new());
        assert_eq!(dropped(&mut was, &[true, true]), Vec::<usize>::new());
        assert_eq!(dropped(&mut was, &[false, false]), vec![0, 1]);
    }

    /// The words of the forward, read by the far taw's own parser: every flag is one it takes, and the
    /// values arrive as they were sent, a value that starts with a dash included.
    #[test]
    fn the_words_of_a_forward_are_read_back_by_the_far_verb() {
        use crate::cli::notification::{Cmd, DeliveryArg, KindArg};
        use clap::Parser;
        #[derive(Parser)]
        struct Far {
            #[command(subcommand)]
            cmd: Cmd,
        }
        let f = Forwarded {
            machine: "studio".into(),
            agent: "-claude".into(),
            location: "garden:1".into(),
            class: Class::Request,
            title: "-claude blocked".into(),
            body: "-Do you want to proceed?".into(),
            dropped: false,
        };
        let words = forward_words(&f, ToastDelivery::Terminal);
        assert_eq!(&words[..2], ["notification", "show"]);
        assert_eq!(
            &words[2..4],
            ["--forwarded=studio", "--delivery=terminal"],
            "the forward carries this Mac's delivery"
        );
        assert_eq!(
            forward_words(&f, ToastDelivery::Herdr)[3],
            "--delivery=herdr"
        );
        let mut argv = vec!["taw".to_string()];
        argv.extend(words[1..].iter().cloned());
        let Cmd::Show {
            title,
            body,
            forwarded,
            agent,
            location,
            kind,
            dropped,
            delivery,
            ..
        } = Far::try_parse_from(&argv).unwrap().cmd
        else {
            panic!("not a show");
        };
        assert_eq!(
            (title, body, forwarded, agent, location),
            (
                "-claude blocked".to_string(),
                Some("-Do you want to proceed?".to_string()),
                Some("studio".to_string()),
                Some("-claude".to_string()),
                Some("garden:1".to_string())
            )
        );
        assert!(matches!(kind, Some(KindArg::Request)));
        assert!(!dropped, "an ordinary toast is not the drop toast");
        assert_eq!(
            delivery,
            Some(DeliveryArg::Terminal),
            "the far verb reads --delivery back"
        );

        // The drop toast has a title and the `--dropped` flag, and the far verb reads the flag back.
        let drop = Forwarded {
            agent: String::new(),
            location: String::new(),
            body: String::new(),
            title: "pi dropped off (last seen just now)".into(),
            dropped: true,
            ..f
        };
        let words = forward_words(&drop, ToastDelivery::Herdr);
        assert_eq!(
            words,
            [
                "notification",
                "show",
                "--forwarded=studio",
                "--delivery=herdr",
                "--dropped",
                "--",
                "pi dropped off (last seen just now)"
            ]
        );
        let mut argv = vec!["taw".to_string()];
        argv.extend(words[1..].iter().cloned());
        let Cmd::Show { dropped, .. } = Far::try_parse_from(&argv).unwrap().cmd else {
            panic!("not a show");
        };
        assert!(dropped, "the far verb reads --dropped");
    }

    /// The text of a toast is data: through the ssh command it reaches the far side's shell as the words given,
    /// whatever it holds, and nothing in it runs. The line is run here as the far shell would run it, with a
    /// stand-in taw that writes the words it was given, and the marker the text would make is never made.
    #[test]
    fn the_text_of_a_forwarded_toast_reaches_the_far_side_as_data() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let marker = dir.path().join("marker");
        let words_file = dir.path().join("words");
        let far_taw = dir.path().join("far-taw");
        std::fs::write(
            &far_taw,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\000' \"$a\"; done > '{}'\n",
                words_file.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&far_taw);
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![
            machine("studio", Some(far_taw.to_str().unwrap())),
            machine("pi", None),
        ];
        let body = format!(
            "#(touch {m}) #{{pane_id}} \"q\" 'q' $(touch {m}) `touch {m}` ; touch {m}\nsecond line",
            m = marker.display()
        );
        let toast = toast_of("claude", &body);
        feed.hub.announce(1, toast.clone(), Left::default());
        feed.record.attached(4242, "tty", "studio");
        let recording = Recording::start();

        feed.announce_toasts();

        let seen = recording.seen();
        assert_eq!(seen.forwards.len(), 1, "forwarded once: {seen:?}");
        let line = lines_run(&seen).remove(0);
        let ran = std::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .status()
            .unwrap();
        assert!(ran.success(), "the line ran the stand-in: {line}");
        let got = std::fs::read(&words_file).unwrap();
        let got: Vec<String> = got
            .split(|b| *b == 0)
            .filter(|w| !w.is_empty())
            .map(|w| String::from_utf8(w.to_vec()).unwrap())
            .collect();
        let want = forward_words(&forwarded("pi", &toast), ToastDelivery::Herdr);
        assert_eq!(got, want, "the far side got the words intact");
        assert!(got.iter().any(|w| w.contains("second line")));
        assert!(!marker.exists(), "nothing in the text ran");
    }

    /// A forward goes to the machine the person is inside, over the app's ssh with the command mode's options:
    /// the exact argv is the one `ssh::command` makes for the words, nothing else.
    #[test]
    fn a_forward_is_the_ssh_command_of_the_far_verbs_for_its_words() {
        // Under each delivery that is forwarded, the flag the forward holds is the one in this Mac's config.
        for (delivery, flag) in [
            (ToastDelivery::Herdr, "--delivery=herdr"),
            (ToastDelivery::Terminal, "--delivery=terminal"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let mut cfg = Config::default();
            cfg.ui.toast.delivery = delivery;
            let paths = paths_in(dir.path());
            let mut feed = feed_over(&tmux, &cfg, &paths);
            feed.machines = vec![machine("studio", None), machine("pi", None)];
            feed.hub
                .announce(1, toast_of("claude", "hi"), Left::default());
            feed.record.attached(4242, "tty", "studio");
            let recording = Recording::start();

            feed.announce_toasts();

            let words = forward_words(&forwarded("pi", &toast_of("claude", "hi")), delivery);
            assert!(
                words.iter().any(|w| w == flag),
                "this Mac's delivery rides the forward: {words:?}"
            );
            let args: Vec<&str> = words.iter().map(String::as_str).collect();
            let expected = ssh::command(&feed.ssh, &paths, &feed.machines[0], Mode::Command, &args);
            let mut argv = vec![expected.get_program().to_string_lossy().into_owned()];
            argv.extend(
                expected
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            );
            let seen = recording.seen().forwards;
            assert_eq!(seen, vec![argv], "{flag}");
            assert!(
                seen[0].iter().any(|w| w.contains(flag)),
                "the recorded argv holds {flag}: {:?}",
                seen[0]
            );
        }
    }

    /// The far verb takes `--delivery` from a forward and from nothing else: only `herdr` and `terminal` (the
    /// sending machine's `off` forwards nothing and its `system` posts on its own desktop, nobody's here), only
    /// with `--forwarded`, and the flag is out of the verb's help.
    #[test]
    fn the_far_verb_takes_only_the_two_deliveries_a_forward_carries() {
        use crate::cli::notification::Cmd;
        use clap::{CommandFactory, Parser};
        #[derive(Parser)]
        struct Far {
            #[command(subcommand)]
            cmd: Cmd,
        }
        let parse = |words: &[&str]| {
            let mut argv = vec!["taw", "show"];
            argv.extend(words);
            Far::try_parse_from(argv)
        };
        assert!(
            parse(&["--forwarded=studio", "--delivery=terminal", "t"]).is_ok(),
            "positive: terminal rides a forward"
        );
        assert!(
            parse(&["--forwarded=studio", "--delivery=herdr", "t"]).is_ok(),
            "positive: herdr rides a forward"
        );
        for refused in ["system", "off"] {
            assert!(
                parse(&["--forwarded=studio", &format!("--delivery={refused}"), "t"]).is_err(),
                "--delivery={refused} is no delivery a forward carries"
            );
        }
        assert!(
            parse(&["--delivery=herdr", "t"]).is_err(),
            "--delivery needs --forwarded"
        );
        let mut far = Far::command();
        let show = far.find_subcommand_mut("show").unwrap();
        let help = show.render_help().to_string();
        assert!(
            help.contains("--body"),
            "positive: the help lists the flags"
        );
        assert!(!help.contains("--delivery"), "the flag is hidden: {help}");
    }

    /// Who does what with a machine's toast: the app's top line in the list, a forward through the machine the
    /// person is inside, nothing for the machine they are inside of; the sound and the push once, and only
    /// for the lease holder (`lease` on the toast), with the machine's name in the push.
    #[test]
    fn a_machines_toast_is_shown_where_the_person_is_and_spoken_by_the_lease_holder() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        let recording = Recording::start();
        let mut toast = toast_of("claude", "Do you want to proceed?");

        // In the list, with the lease: the top line, the sound once, the push once.
        toast.lease = true;
        feed.hub.announce(0, toast.clone(), Left::default());
        feed.announce_toasts();
        let seen = recording.seen();
        assert_eq!(
            top_of(&mut feed, &cfg).as_deref(),
            Some("studio: claude blocked \u{b7} garden:1 \u{b7} Do you want to proceed?")
        );
        assert_eq!(seen.sounds.len(), 1);
        assert_eq!(
            seen.pushes,
            vec![(Some("studio".to_string()), Some("%31".to_string()))]
        );
        assert!(seen.forwards.is_empty());

        // Without the lease (another app's): the top line is the same, no sound, no push.
        toast.lease = false;
        feed.hub.announce(0, toast.clone(), Left::default());
        feed.announce_toasts();
        assert!(top_of(&mut feed, &cfg).is_some(), "shown for this person");
        let seen = recording.seen();
        assert_eq!((seen.sounds.len(), seen.pushes.len()), (1, 1), "still one");

        // Inside another machine: forwarded through it, once; inside the machine it is about: nothing is
        // forwarded, and the lease holder still speaks.
        feed.record.attached(4242, "tty", "pi");
        feed.hub.announce(0, toast.clone(), Left::default());
        feed.announce_toasts();
        assert_eq!(recording.seen().forwards.len(), 1, "positive: through pi");
        assert!(lines_run(&recording.seen())[0].contains("--forwarded=studio"));
        feed.record.attached(4242, "tty", "studio");
        toast.lease = true;
        feed.hub.announce(0, toast.clone(), Left::default());
        feed.announce_toasts();
        let seen = recording.seen();
        assert_eq!(seen.forwards.len(), 1, "none through studio about studio");
        assert_eq!(
            (seen.sounds.len(), seen.pushes.len()),
            (2, 2),
            "spoken once more"
        );
        assert_eq!(top_of(&mut feed, &cfg), None, "and not on the top line");

        // Inside this Mac: a machine's toast is drawn here, in this process, with the machine ahead of the place.
        feed.record.attached(4242, "tty", "");
        toast.lease = false;
        feed.hub.announce(1, toast, Left::default());
        feed.announce_toasts();
        let text = drawn(&log);
        assert_eq!(text.len(), 1, "drawn once: {text:?}");
        assert!(text[0].contains("pi \u{b7} garden:1"), "{}", text[0]);
        assert_eq!(
            recording.seen().forwards.len(),
            1,
            "none forwarded from this Mac"
        );
    }

    /// With no machine listed (or only this Mac's own tmux server) a toast of this Mac names no machine on the
    /// app's top line: the toast is waited for first (the top line is there), then it is read.
    #[test]
    fn this_macs_toast_names_no_machine_when_no_other_machine_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let _recording = Recording::start();
        assert!(files::offer_lease(
            &paths,
            &conn::holder_word(),
            files::now_ms()
        ));
        files::append_event(&paths, &Down::Toast(toast_of("claude", "mine")));
        feed.announce_toasts();
        let top = top_of(&mut feed, &cfg).expect("the toast is on the top line");
        assert!(top.ends_with("mine"), "positive half: {top}");
        assert!(!top.contains(this_machine_word()), "{top}");
        assert_eq!(top, toast_of("claude", "mine").text);
        // And with another machine listed it is named, as ever.
        feed.machines = vec![machine("studio", None)];
        files::append_event(&paths, &Down::Toast(toast_of("claude", "again")));
        feed.announce_toasts();
        let top = top_of(&mut feed, &cfg).expect("the second toast");
        assert!(
            top.starts_with(&format!("{}: ", this_machine_word())),
            "{top}"
        );
    }

    /// A toast of this machine drawn on another machine's status line names this machine by its short host
    /// name, never `this machine` or `this Mac`, which there would name that machine.
    #[test]
    fn this_machines_toast_drawn_elsewhere_is_named_by_its_host() {
        assert_eq!(far_word("studio"), "studio");
        assert_eq!(far_word(""), this_machine_word());
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None)];
        let recording = Recording::start();
        feed.record.attached(4242, "tty", "studio");
        files::append_event(&paths, &Down::Toast(toast_of("claude", "there")));
        feed.announce_toasts();
        let seen = recording.seen();
        let host = crate::machine::feed::host_name();
        let run = lines_run(&seen);
        assert_eq!(run.len(), 1, "{run:?}");
        if !host.is_empty() {
            assert!(run[0].contains(&format!("--forwarded={host}")), "{run:?}");
            assert!(!run[0].contains("--forwarded=this "), "{run:?}");
        }
    }

    /// This Mac's toasts are the lines its watcher appended, read by a tail that began with the app; the push
    /// names no machine (`Some("")`), the sound and the push are for the app that holds this Mac's lease, and
    /// inside this Mac they show nothing (the watcher drew it).
    #[test]
    fn this_macs_toast_is_read_from_its_file_and_spoken_by_the_app_that_holds_its_lease() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        files::append_event(&paths, &Down::Toast(toast_of("claude", "before the app")));
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None)];
        let recording = Recording::start();
        let line = |body: &str| files::append_event(&paths, &Down::Toast(toast_of("claude", body)));
        let hold = |holder: &str| assert!(files::offer_lease(&paths, holder, files::now_ms()));

        // This app holds the lease: the toast is on the top line, the sound and the push happen once; what was
        // in the file when the app started did not.
        hold(&conn::holder_word());
        line("mine");
        feed.announce_toasts();
        let seen = recording.seen();
        let top = top_of(&mut feed, &cfg).unwrap();
        assert!(
            top.starts_with(&format!("{}: ", this_machine_word())),
            "{top}"
        );
        assert!(top.ends_with("mine"), "{top}");
        assert_eq!(seen.sounds.len(), 1);
        assert_eq!(
            seen.pushes,
            vec![(Some(String::new()), Some("%31".to_string()))]
        );

        // Another app holds it (a fresh lease of another holder): shown, not spoken.
        std::fs::remove_file(paths.machines_dir().join("lease.json")).unwrap();
        hold("another-4242");
        line("theirs");
        feed.announce_toasts();
        assert!(top_of(&mut feed, &cfg).unwrap().ends_with("theirs"));
        let seen = recording.seen();
        assert_eq!(
            (seen.sounds.len(), seen.pushes.len()),
            (1, 1),
            "no second voice"
        );

        // Inside another machine it is forwarded through it, with this Mac's host name, not its own word.
        feed.record.attached(4242, "tty", "studio");
        line("elsewhere");
        feed.announce_toasts();
        let seen = recording.seen();
        assert_eq!(seen.forwards.len(), 1);
        assert!(
            !lines_run(&seen)[0].contains(&format!("--forwarded={}", this_machine_word())),
            "{:?}",
            lines_run(&seen)
        );

        // Inside this Mac: nothing, here or through anyone.
        feed.record.attached(4242, "tty", "");
        line("in this mac");
        feed.announce_toasts();
        assert_eq!(recording.seen().forwards.len(), 1);
        assert_eq!(drawn(&log), Vec::<String>::new());
        assert_eq!(top_of(&mut feed, &cfg), None);
    }

    /// `[ui.toast] delivery = "off"` shows a toast nowhere (the toast is seen first with `herdr`), and the lease
    /// holder still plays the sound and runs the push, once.
    #[test]
    fn delivery_off_shows_no_toast_and_still_speaks_for_the_lease_holder() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let mut cfg = Config::default();
        let paths = paths_in(dir.path());
        let recording = Recording::start();
        let mut toast = toast_of("claude", "hi");
        toast.lease = true;
        {
            let mut feed = feed_over(&tmux, &cfg, &paths);
            feed.machines = vec![machine("studio", None)];
            feed.hub.announce(0, toast.clone(), Left::default());
            feed.announce_toasts();
            assert!(
                top_of(&mut feed, &cfg).is_some(),
                "positive: herdr shows it"
            );
        }
        cfg.ui.toast.delivery = ToastDelivery::Off;
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None)];
        feed.hub.announce(0, toast, Left::default());
        feed.announce_toasts();

        assert_eq!(top_of(&mut feed, &cfg), None);
        let seen = recording.seen();
        assert_eq!(
            (seen.sounds.len(), seen.pushes.len()),
            (2, 2),
            "one more of each"
        );
        assert!(seen.forwards.is_empty());
    }

    /// A machine that was live and is not says so once: on the top line in the list, through the machine the
    /// person is inside (a title and nothing else), in this process inside this Mac; never for the machine they
    /// are inside, and not again until it has been live again.
    #[test]
    fn a_machine_that_drops_off_is_said_once_where_the_person_is() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        let recording = Recording::start();
        let words = "pi dropped off (last seen just now)";

        recording.live(&[true, true]);
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg), None, "both live: nothing");
        recording.live(&[true, false]);
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg).as_deref(), Some(words));
        feed.announce_toasts();
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg), None, "not twice");

        // Live again, then dropped again, inside studio: forwarded through studio, title only.
        recording.live(&[true, true]);
        feed.announce_toasts();
        feed.record.attached(4242, "tty", "studio");
        recording.live(&[true, false]);
        feed.announce_toasts();
        feed.announce_toasts();
        let seen = recording.seen();
        assert_eq!(seen.forwards.len(), 1, "once, through studio");
        let line = lines_run(&seen).remove(0);
        assert!(line.contains("--forwarded=pi"), "{line}");
        assert!(line.contains(words), "{line}");
        assert!(!line.contains("--agent"), "{line}");
        assert!(
            line.contains("--dropped"),
            "the machine it goes through must be told it is the drop toast: {line}"
        );

        // Studio drops while the person is inside it: nothing is said.
        recording.live(&[true, true]);
        feed.announce_toasts();
        recording.live(&[false, true]);
        feed.announce_toasts();
        assert_eq!(recording.seen().forwards.len(), 1);
        assert_eq!(top_of(&mut feed, &cfg), None);
        // Back in the list, it is still not said: it was seen to go while the person was inside.
        feed.record.detached();
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg), None);

        // Inside this Mac: drawn here, the title alone.
        feed.record.attached(4242, "tty", "");
        recording.live(&[true, true]);
        feed.announce_toasts();
        recording.live(&[true, false]);
        feed.announce_toasts();
        let text = drawn(&log);
        assert_eq!(text.len(), 1, "{text:?}");
        assert!(text[0].contains(words), "{}", text[0]);
        assert!(
            !text[0].contains("pi: "),
            "a drop toast is its own words, with no machine ahead of them: {}",
            text[0]
        );
        assert_eq!(recording.seen().sounds.len(), 0, "a drop makes no sound");
    }

    /// An app that does not keep the connections cannot tell a machine that dropped off from a keeper that
    /// is gone: it says nothing of liveness. A keeper that is not there at all (no machines) says none either.
    #[test]
    fn an_app_that_does_not_keep_the_connections_says_no_machine_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.machines = vec![machine("studio", None)];
        assert_eq!(feed.live_flags(), None);
        feed.announce_toasts();
        assert_eq!(top_of(&mut feed, &cfg), None);
    }

    /// An ssh stand-in that writes its own pid to `pid`, then `behaviour`; ends what it started by that pid
    /// when the test is over, a failing assert included.
    struct Far {
        bin: PathBuf,
        pid: PathBuf,
        release: PathBuf,
    }

    impl Far {
        fn new(dir: &Path, behaviour: &str) -> Far {
            let far = Far {
                bin: dir.join("far-ssh"),
                pid: dir.join("far-pid"),
                release: dir.join("far-release"),
            };
            std::fs::write(
                &far.bin,
                format!(
                    "#!/bin/sh\necho $$ > '{}.tmp' && mv '{}.tmp' '{}'\n{behaviour}\n",
                    far.pid.display(),
                    far.pid.display(),
                    far.pid.display(),
                ),
            )
            .unwrap();
            crate::test_exec::make_executable(&far.bin);
            far
        }

        fn pid(&self) -> Option<libc::pid_t> {
            std::fs::read_to_string(&self.pid).ok()?.trim().parse().ok()
        }

        fn alive(&self) -> bool {
            // SAFETY: signal 0 only asks whether the process this test's stand-in made is there.
            self.pid()
                .is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
        }

        fn wait(&self, what: &str, until: impl Fn(&Far) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !until(self) {
                assert!(
                    Instant::now() < deadline,
                    "{what}: pid {:?}, alive {}",
                    self.pid(),
                    self.alive()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Far {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, "");
            if let Some(pid) = self.pid() {
                // SAFETY: the pid of the stand-in this test started, written by it.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    /// A forward never holds the app: with the machine's ssh still running, the app goes on taking toasts, and
    /// the ssh is let be until it ends by itself. The ssh was seen to run first.
    #[test]
    fn a_forward_that_is_still_running_does_not_hold_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let far = Far::new(
            dir.path(),
            &format!(
                "while [ ! -e '{}' ]; do sleep 0.05; done",
                dir.path().join("far-release").display()
            ),
        );
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.ssh = ssh::Ssh {
            bin: far.bin.clone(),
        };
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        feed.record.attached(4242, "tty", "studio");
        feed.hub
            .announce(1, toast_of("claude", "hi"), Left::default());

        feed.announce_toasts();

        far.wait("the forward never ran the ssh", |far| far.alive());
        // The ssh is running, and the app takes the next toast, in the list now, and shows it.
        feed.record.detached();
        feed.hub
            .announce(1, toast_of("claude", "next"), Left::default());
        feed.announce_toasts();
        assert!(top_of(&mut feed, &cfg).unwrap().ends_with("next"));
        assert!(far.alive(), "the ssh was not waited for or ended");
        std::fs::write(&far.release, "").unwrap();
        far.wait("the ssh did not end once released", |far| !far.alive());
    }

    /// An ssh stand-in that adds its pid to a line of `pids` and then waits for `release`; what it started is
    /// ended by those pids when the test is over, a failing assert included.
    struct Crowd {
        bin: PathBuf,
        pids: PathBuf,
        release: PathBuf,
    }

    impl Crowd {
        fn new(dir: &Path) -> Crowd {
            let crowd = Crowd {
                bin: dir.join("crowd-ssh"),
                pids: dir.join("crowd-pids"),
                release: dir.join("crowd-release"),
            };
            std::fs::write(
                &crowd.bin,
                format!(
                    "#!/bin/sh\necho $$ >> '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n",
                    crowd.pids.display(),
                    crowd.release.display()
                ),
            )
            .unwrap();
            crate::test_exec::make_executable(&crowd.bin);
            crowd
        }

        fn started(&self) -> usize {
            std::fs::read_to_string(&self.pids)
                .unwrap_or_default()
                .lines()
                .count()
        }

        fn wait_for(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while self.started() < count {
                assert!(
                    Instant::now() < deadline,
                    "waiting for {count} stand-ins, {} started",
                    self.started()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Crowd {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, "");
            for line in std::fs::read_to_string(&self.pids)
                .unwrap_or_default()
                .lines()
            {
                if let Ok(pid) = line.trim().parse::<libc::pid_t>() {
                    // SAFETY: the pid of a stand-in this test started, written by it.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }

    /// At most three forwards run at once: four toasts at once start three ssh, the fourth is lost; once they
    /// have ended a new toast is forwarded again.
    #[test]
    fn at_most_three_forwards_run_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let crowd = Crowd::new(dir.path());
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.ssh = ssh::Ssh {
            bin: crowd.bin.clone(),
        };
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        feed.record.attached(4242, "tty", "studio");
        for body in ["one", "two", "three", "four"] {
            feed.hub
                .announce(1, toast_of("claude", body), Left::default());
        }

        feed.announce_toasts();

        crowd.wait_for(3);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(crowd.started(), 3, "the fourth forward was started");

        // Positive: once they have ended, the room is there again.
        std::fs::write(&crowd.release, "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while locked(&feed.toasts.forwards).running > 0 {
            assert!(Instant::now() < deadline, "the forwards never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_file(&crowd.release).unwrap();
        feed.hub
            .announce(1, toast_of("claude", "five"), Left::default());
        feed.announce_toasts();
        crowd.wait_for(4);
    }

    /// The app does not leave an ssh behind: dropping the toaster ends a forward that is still running, by its
    /// pid (its bound is made long here, so only the drop can end it before the end of the test).
    #[test]
    fn dropping_the_toaster_ends_a_forward_that_is_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let far = Far::new(dir.path(), "exec sleep 600");
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.ssh = ssh::Ssh {
            bin: far.bin.clone(),
        };
        feed.toasts.bound = Duration::from_secs(600);
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        feed.record.attached(4242, "tty", "studio");
        feed.hub
            .announce(1, toast_of("claude", "hi"), Left::default());
        feed.announce_toasts();
        far.wait("the forward never ran the ssh", |far| far.alive());

        drop(feed);

        far.wait("the ssh outlived the toaster", |far| !far.alive());
    }

    /// A machine that hangs loses the toast: the ssh is ended when the bound is up, by its pid.
    #[test]
    fn a_forward_to_a_machine_that_hangs_is_ended_at_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let far = Far::new(dir.path(), "exec sleep 600");
        let mut command = Command::new(&far.bin);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        send(command, Duration::from_millis(500), &Arc::default());

        far.wait("the ssh never ran", |far| far.pid().is_some());
        far.wait("the ssh was not ended at its bound", |far| !far.alive());
    }

    /// A machine whose ssh fails at once (it does not know the flags, or cannot be reached) loses the toast and
    /// nothing else: the ssh ran, and the app goes on.
    #[test]
    fn a_forward_that_fails_loses_the_toast_and_breaks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let far = Far::new(dir.path(), "exit 255");
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.ssh = ssh::Ssh {
            bin: far.bin.clone(),
        };
        feed.machines = vec![machine("studio", None), machine("pi", None)];
        feed.record.attached(4242, "tty", "studio");
        feed.hub
            .announce(1, toast_of("claude", "hi"), Left::default());

        feed.announce_toasts();

        far.wait("the forward never ran the ssh", |far| far.pid().is_some());
        feed.record.detached();
        feed.hub
            .announce(1, toast_of("claude", "after"), Left::default());
        feed.announce_toasts();
        assert!(top_of(&mut feed, &cfg).unwrap().ends_with("after"));
    }

    /// What the toaster keeps by a machine's place (was it live, did it list a session) follows the machine to
    /// its place in the new list; a machine that is gone leaves nothing behind, and one that is new starts as not
    /// live.
    #[test]
    fn what_is_kept_by_a_machines_place_follows_the_machine_to_its_new_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut toaster = Toaster::new(&paths_in(dir.path()), Path::new("/nonexistent/taw"));
        toaster.live = vec![true, false, true];
        toaster.had_sessions = ["a", "b", "c"].map(String::from).into();

        // a is gone, b moved to the second place, c to the first: the list is now c, b.
        toaster.machines_changed(
            &[None, Some(1), Some(0)],
            &[machine("c", None), machine("b", None)],
        );

        assert_eq!(toaster.live, vec![true, false], "c was live and b was not");
        assert_eq!(
            toaster.had_sessions,
            ["b", "c"].map(String::from).into(),
            "a is no longer listed"
        );

        // A machine that is new has no place in the old list: it is not live, and the others stay where they went.
        toaster.machines_changed(
            &[Some(1), Some(0)],
            &[machine("b", None), machine("c", None), machine("d", None)],
        );

        assert_eq!(
            toaster.live,
            vec![false, true, false],
            "d is new, so not live"
        );
    }
}

#[cfg(test)]
mod no_session_news_tests {
    use super::*;
    use crate::tui::data::MachineHeader;
    use crate::tui::fixtures::{Counts, snapshot, space};

    fn over(live: bool, stated: bool, with_session: bool) -> Snapshot {
        let mut snap = snapshot();
        snap.spaces.clear();
        snap.panes.clear();
        snap.agents.clear();
        snap.machines = vec![MachineHeader {
            name: "studio".into(),
            live,
            stated,
            ..MachineHeader::default()
        }];
        if with_session {
            let mut row = space(1, "/tmp/garden", "main", Counts::default());
            row.machine = Some(0);
            snap.spaces.push(row);
        }
        snap
    }

    /// A machine that listed a session and lists none now, live and stated, is told of once.
    #[test]
    fn a_machine_that_loses_its_last_session_is_told_of_once() {
        let mut had = std::collections::HashSet::new();
        assert!(no_session_news(&mut had, &over(true, true, true)).is_empty());
        assert_eq!(
            no_session_news(&mut had, &over(true, true, false)),
            vec!["studio has no tmux session now: Enter on its row starts one".to_string()]
        );
        assert!(no_session_news(&mut had, &over(true, true, false)).is_empty());
    }

    /// One that is away, or has said nothing, may only be out of sight: no word, and the next one still counts.
    #[test]
    fn a_machine_out_of_sight_is_not_told_of() {
        let mut had = std::collections::HashSet::new();
        assert!(no_session_news(&mut had, &over(true, true, true)).is_empty());
        assert!(no_session_news(&mut had, &over(false, true, false)).is_empty());
        assert!(no_session_news(&mut had, &over(true, false, false)).is_empty());
        assert_eq!(no_session_news(&mut had, &over(true, true, false)).len(), 1);
    }
}
