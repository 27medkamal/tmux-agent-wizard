//! Every machine of `[machines]` as the app holds them: one connection each ([`super::conn`]), what each
//! last said, and the files that tell the other apps and the popup.
//!
//! One app on a machine keeps the connections: the one that holds the keeper lock
//! ([`files::take_keeper`]). It writes each machine's file whenever its state or its words change, and at
//! least every `lease_every` while the machine is connected, so that `last_seen` moves while the machine
//! is quiet. Any other app only reads the files, and tries the lock once a second: when the keeper goes
//! it takes over.

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use super::conn::{self, ConnCtx, ConnEvent, ConnHandle};
use super::files::{self, Keeper, MachineFile};
use super::wire::{self, Left, Toast, Up};
use super::{Machine, MachineView, words};

/// How often an app that only reads the files looks at them.
const FOLLOW_READ: Duration = Duration::from_millis(200);

/// How often an app that only reads the files tries to become the keeper.
const FOLLOW_TAKE: Duration = Duration::from_secs(1);

/// How many tries at the lock in a row, with no app holding it, must fail before it is said that this
/// app can never take it. More than one, so that two apps that try at the same moment are not taken for it.
const CANNOT_KEEP_AFTER: u32 = 3;

/// Notices a follower that cannot ever become the keeper (the state folder cannot be written, or the lock
/// file is a folder): nobody holds the lock and the follower still cannot take it, again and again.
#[derive(Default)]
struct CannotKeep {
    misses: u32,
    warned: bool,
}

impl CannotKeep {
    /// After a try at the lock that failed: `another_holds` is whether an app holds it. `true` once, on
    /// the failure that makes it plain that this app never will: the caller says so.
    fn after_a_failed_try(&mut self, another_holds: bool) -> bool {
        if another_holds {
            self.misses = 0;
            return false;
        }
        self.misses = self.misses.saturating_add(1);
        let say = self.misses >= CANNOT_KEEP_AFTER && !self.warned;
        self.warned |= say;
        say
    }
}

/// What the app knows of one machine.
struct Slot {
    view: MachineView,
    received_at: Option<u64>,
    taw_version: Option<String>,
    protocol: Option<u32>,
    /// A connection is up (a hello was heard and it has not dropped).
    connected: bool,
    /// What the file says is not what this is.
    dirty: bool,
}

impl Slot {
    /// From the machine's file as an earlier run left it, or from nothing: not connected, and what
    /// it last said is kept to be shown greyed.
    fn from_file(name: &str, file: Option<&MachineFile>, now_ms: u64) -> Slot {
        let mut view = files::view_of(file, name, true, now_ms);
        view.live = false;
        // Whatever the file said of the machine's connection was another run's.
        view.words = words::connecting(
            view.last_seen
                .map(|seen| now_ms.saturating_sub(seen) / 1000),
        );
        Slot {
            received_at: file.and_then(|file| file.received_at),
            taw_version: file.and_then(|file| file.taw_version.clone()),
            protocol: file.and_then(|file| file.protocol),
            view,
            connected: false,
            dirty: false,
        }
    }

    fn file(&self) -> MachineFile {
        MachineFile {
            name: self.view.name.clone(),
            state: self.view.state.clone(),
            received_at: self.received_at,
            last_seen: self.view.last_seen,
            live: self.connected,
            words: self.view.words.clone(),
            taw_version: self.taw_version.clone(),
            protocol: self.protocol,
            this_server: self.view.this_server,
        }
    }
}

/// What the app is doing about the machines.
enum Role {
    /// There are no machines: nothing is started and nothing is written.
    Idle,
    /// This app holds the keeper lock and the connections.
    Keeper {
        /// Dropped first: the connections end before the lock is let go.
        handles: Vec<ConnHandle>,
        events: Receiver<ConnEvent>,
        _lock: Keeper,
        /// Says this app is still there, so that its lease on this machine counts only while it runs: an app
        /// started after it ends takes the lease at once, and this app's own clean end gives it up
        /// ([`files::HolderLock`]). `None` while another process holds it (a feed of this machine's own server, when
        /// `[machines]` lists it, which hears this app's lease lines): taken again at each poll until it is this app's.
        alive: Option<files::HolderLock>,
        /// When the files were last written whole and the lease last offered.
        beat: Instant,
    },
    /// Another app keeps them: this one reads the files.
    Follower {
        last_read: Option<Instant>,
        last_take: Instant,
        cannot_keep: CannotKeep,
    },
}

/// Every machine of `[machines]`, as the app holds them.
pub struct Machines {
    ctx: ConnCtx,
    machines: Vec<Machine>,
    slots: Vec<Slot>,
    role: Role,
    toasts: Vec<(usize, Toast)>,
    lefts: Vec<(usize, Left)>,
    /// The last walk said to each machine, in the config's order. Kept in every role, so that an app
    /// that takes over the connections says it to the new ones at once.
    walks: Vec<Option<wire::WalkLine>>,
}

impl Machines {
    /// The app that keeps the connections: takes the keeper lock and connects to every machine. When
    /// another app holds the lock this one follows it ([`Machines::follow`]). With no machines nothing
    /// is started, written or locked.
    pub fn start(ctx: ConnCtx, machines: Vec<Machine>) -> Machines {
        if machines.is_empty() {
            return Machines::idle(ctx);
        }
        match files::take_keeper(&ctx.paths) {
            Some(lock) => Machines::keeping(ctx, machines, lock),
            None => Machines::follow(ctx, machines),
        }
    }

    /// An app that reads what the keeper writes, and tries every second to become the keeper.
    pub fn follow(ctx: ConnCtx, machines: Vec<Machine>) -> Machines {
        if machines.is_empty() {
            return Machines::idle(ctx);
        }
        let now_ms = files::now_ms();
        let slots = machines
            .iter()
            .map(|m| Slot::from_file(&m.name, files::read(&ctx.paths, &m.name).as_ref(), now_ms))
            .collect();
        let walks = vec![None; machines.len()];
        Machines {
            ctx,
            machines,
            slots,
            role: Role::Follower {
                last_read: None,
                last_take: Instant::now(),
                cannot_keep: CannotKeep::default(),
            },
            toasts: Vec::new(),
            lefts: Vec::new(),
            walks,
        }
    }

    fn idle(ctx: ConnCtx) -> Machines {
        Machines {
            ctx,
            machines: Vec::new(),
            slots: Vec::new(),
            role: Role::Idle,
            toasts: Vec::new(),
            lefts: Vec::new(),
            walks: Vec::new(),
        }
    }

    fn keeping(ctx: ConnCtx, machines: Vec<Machine>, lock: Keeper) -> Machines {
        let now_ms = files::now_ms();
        let slots: Vec<Slot> = machines
            .iter()
            .map(|m| Slot::from_file(&m.name, files::read(&ctx.paths, &m.name).as_ref(), now_ms))
            .collect();
        let walks = vec![None; machines.len()];
        let mut this = Machines {
            ctx,
            machines,
            slots,
            role: Role::Idle,
            toasts: Vec::new(),
            lefts: Vec::new(),
            walks,
        };
        this.become_keeper(lock);
        this
    }

    /// Starts the connections, from the slots as they are, and offers this machine's lease.
    fn become_keeper(&mut self, lock: Keeper) {
        let (tx, events) = channel();
        let handles: Vec<ConnHandle> = self
            .machines
            .iter()
            .zip(&self.slots)
            .enumerate()
            .map(|(index, (m, slot))| {
                conn::spawn_since(
                    self.ctx.clone(),
                    index,
                    m.clone(),
                    tx.clone(),
                    slot.view.last_seen.unwrap_or(0),
                )
            })
            .collect();
        // What was said to each machine before (as a follower, or by the app that kept them): said again to
        // the new connection, which keeps it for its lease lines.
        for (handle, walk) in handles.iter().zip(&self.walks) {
            if let Some(walk) = walk {
                handle.send(Up::Walk(walk.clone()));
            }
        }
        // The files this app inherits say what the keeper before it knew: connected, and live. This app has
        // no connection yet, so every file is written again at the next poll as not live, with the words for
        // connecting, until a hello says otherwise.
        for slot in &mut self.slots {
            slot.dirty = true;
        }
        self.role = Role::Keeper {
            handles,
            events,
            _lock: lock,
            alive: files::take_holder_lock(&self.ctx.paths, &self.ctx.holder),
            beat: Instant::now(),
        };
        self.offer_lease();
    }

    /// Offers this machine's own lease, as the keeper does at start and every 5 seconds: whether this
    /// app holds it now. An app that is not the keeper never offers; it only says what it finds.
    pub fn offer_lease(&mut self) -> bool {
        let now_ms = files::now_ms();
        match self.role {
            Role::Keeper { .. } => files::offer_lease(&self.ctx.paths, &self.ctx.holder, now_ms),
            _ => {
                files::lease_fresh(&self.ctx.paths, now_ms)
                    && files::read_lease(&self.ctx.paths)
                        .is_some_and(|lease| lease.holder == self.ctx.holder)
            }
        }
    }

    /// Takes in what the connections said (or, for a follower, what the files say), writes the files
    /// that changed. `true` when anything the list shows changed, or there is a toast or a left to take.
    pub fn poll(&mut self, now: Instant) -> bool {
        #[cfg(test)]
        self.deliver_scheduled();
        match self.role {
            Role::Idle => false,
            Role::Keeper { .. } => self.poll_keeper(now),
            Role::Follower { .. } => self.poll_follower(now),
        }
    }

    fn poll_keeper(&mut self, now: Instant) -> bool {
        // The lock that says this app is there, tried again while a feed of this machine's own server has it: once
        // that feed ends this app holds it, and an app started after this one ends takes the lease at once.
        if let Role::Keeper {
            alive: alive @ None,
            ..
        } = &mut self.role
        {
            *alive = files::take_holder_lock(&self.ctx.paths, &self.ctx.holder);
        }
        let mut changed = false;
        let mut received = Vec::new();
        if let Role::Keeper { events, .. } = &self.role {
            while let Ok(event) = events.try_recv() {
                received.push(event);
            }
        }
        for event in received {
            changed |= self.apply(event);
        }
        // What the connections heard last, so that `last_seen` moves while a machine is quiet.
        let mut beat = false;
        if let Role::Keeper {
            handles, beat: at, ..
        } = &mut self.role
        {
            for (slot, handle) in self.slots.iter_mut().zip(handles.iter()) {
                let heard = handle.heard_ms();
                if heard != 0 {
                    slot.view.last_seen = Some(heard);
                }
            }
            if now.duration_since(*at) >= self.ctx.timing.lease_every {
                *at = now;
                beat = true;
            }
        }
        if beat {
            self.offer_lease();
            for slot in self.slots.iter_mut().filter(|slot| slot.connected) {
                slot.dirty = true;
            }
        }
        for slot in self.slots.iter_mut().filter(|slot| slot.dirty) {
            files::write_machine_file(&self.ctx.paths, &slot.file());
            slot.dirty = false;
        }
        changed
    }

    /// One event of a connection taken in. `true` when the list changes by it.
    fn apply(&mut self, event: ConnEvent) -> bool {
        match event {
            ConnEvent::State { machine, state } => {
                let Some(slot) = self.slots.get_mut(machine) else {
                    return false;
                };
                slot.view.state = Some(state);
                slot.received_at = Some(files::now_ms());
                slot.dirty = true;
                true
            }
            ConnEvent::Words {
                machine,
                live,
                words,
            } => {
                let Some(slot) = self.slots.get_mut(machine) else {
                    return false;
                };
                slot.connected = live;
                slot.view.live = live;
                slot.view.words = words;
                slot.dirty = true;
                true
            }
            ConnEvent::Hello { machine, hello } => {
                let this_server = conn::is_this_server(self.ctx.local_server.as_deref(), &hello);
                let Some(slot) = self.slots.get_mut(machine) else {
                    return false;
                };
                slot.taw_version = Some(hello.taw);
                slot.protocol = Some(hello.protocol);
                slot.view.this_server = this_server;
                if this_server {
                    // Nothing of this machine is listed twice.
                    slot.view.state = None;
                    slot.received_at = None;
                }
                slot.dirty = true;
                true
            }
            ConnEvent::Toast { machine, toast } => {
                self.toasts.push((machine, toast));
                true
            }
            ConnEvent::Left { machine, left } => {
                self.lefts.push((machine, left));
                true
            }
        }
    }

    fn poll_follower(&mut self, now: Instant) -> bool {
        let mut changed = false;
        let Role::Follower {
            last_read,
            last_take,
            cannot_keep,
        } = &mut self.role
        else {
            return false;
        };
        let read_due = last_read.is_none_or(|at| now.duration_since(at) >= FOLLOW_READ);
        let take_due = now.duration_since(*last_take) >= FOLLOW_TAKE;
        if read_due {
            *last_read = Some(now);
        }
        if take_due {
            *last_take = now;
        }
        if read_due {
            let now_ms = files::now_ms();
            let keeper = files::keeper_alive(&self.ctx.paths);
            for (m, slot) in self.machines.iter().zip(self.slots.iter_mut()) {
                let file = files::read(&self.ctx.paths, &m.name);
                let view = files::view_of(file.as_ref(), &m.name, keeper, now_ms);
                if view != slot.view {
                    slot.view = view;
                    changed = true;
                }
            }
        }
        let lock = if take_due {
            let lock = files::take_keeper(&self.ctx.paths);
            if lock.is_none()
                && cannot_keep.after_a_failed_try(files::keeper_alive(&self.ctx.paths))
            {
                log::warn!(
                    "no app keeps the machines and this one cannot take the lock {}: is the state folder writable, and is that a file?",
                    self.ctx.paths.keeper_lock_file().display()
                );
            }
            lock
        } else {
            None
        };
        if let Some(lock) = lock {
            // The files were read just now, or are read on the next turn: the slots start from them.
            let now_ms = files::now_ms();
            for (m, slot) in self.machines.iter().zip(self.slots.iter_mut()) {
                *slot = Slot::from_file(
                    &m.name,
                    files::read(&self.ctx.paths, &m.name).as_ref(),
                    now_ms,
                );
            }
            self.become_keeper(lock);
            changed = true;
        }
        changed
    }

    /// What the list knows of each machine, in the config's order. A machine counts as live only while
    /// it is connected and heard from within the last 15 seconds.
    pub fn views(&self) -> Vec<MachineView> {
        let now_ms = files::now_ms();
        self.slots
            .iter()
            .map(|slot| {
                let mut view = slot.view.clone();
                if matches!(self.role, Role::Keeper { .. }) {
                    view.live = keeper_sees_live(slot, now_ms);
                }
                view
            })
            .collect()
    }

    /// Whether each machine is live now, in the config's order, as [`Machines::views`] says it; `None` for an
    /// app that does not keep the connections: a follower reads liveness off a file, and a keeper that is gone
    /// makes every machine look away there, which is no machine dropping off.
    pub fn live_flags(&self) -> Option<Vec<bool>> {
        if !matches!(self.role, Role::Keeper { .. }) {
            return None;
        }
        let now_ms = files::now_ms();
        Some(
            self.slots
                .iter()
                .map(|slot| keeper_sees_live(slot, now_ms))
                .collect(),
        )
    }

    /// The toasts the machines announced since the last call.
    pub fn toasts(&mut self) -> Vec<(usize, Toast)> {
        std::mem::take(&mut self.toasts)
    }

    /// The attaches that ended on a machine since the last call.
    pub fn lefts(&mut self) -> Vec<(usize, Left)> {
        std::mem::take(&mut self.lefts)
    }

    /// The list was shown: every machine that waits tries now.
    pub fn wake(&self) {
        if let Role::Keeper { handles, .. } = &self.role {
            for handle in handles {
                handle.wake();
            }
        }
    }

    /// Says the walk to a machine: `you` is the name this app gives it, `stops` the ids of every
    /// machine's stops in this app's names. It is kept in every role and said again with every lease line.
    /// Only the keeper has a connection to say it on; a follower says it to the new connections when it
    /// takes them over.
    pub fn send_walk(&mut self, machine: usize, you: &str, stops: &[String]) {
        let walk = wire::WalkLine {
            you: you.to_string(),
            stops: stops.to_vec(),
        };
        if let Some(kept) = self.walks.get_mut(machine) {
            *kept = Some(walk.clone());
        }
        if let Role::Keeper { handles, .. } = &self.role
            && let Some(handle) = handles.get(machine)
        {
            handle.send(Up::Walk(walk));
        }
    }
}

#[cfg(test)]
impl Machines {
    /// The walk last said to `machine`, as it is kept for its lease lines.
    pub fn walk_said(&self, machine: usize) -> Option<&wire::WalkLine> {
        self.walks.get(machine)?.as_ref()
    }
}

#[cfg(test)]
thread_local! {
    /// The lines `announce_on_poll` schedules for this test's thread: polls still to come, machine, line.
    static ON_POLL: std::cell::RefCell<Vec<(usize, usize, Left)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
impl Machines {
    /// Has `left` arrive for `machine` on the `n`th poll from now (the first is `n` = 1), as an attach end that
    /// comes while someone waits for it would. The schedule belongs to the test's thread, not to a `Machines`:
    /// whichever polls on this thread delivers it.
    pub(crate) fn announce_on_poll(n: usize, machine: usize, left: Left) {
        ON_POLL.with(|scheduled| scheduled.borrow_mut().push((n, machine, left)));
    }

    fn deliver_scheduled(&mut self) {
        let due = ON_POLL.with(|scheduled| {
            let mut scheduled = scheduled.borrow_mut();
            for line in scheduled.iter_mut() {
                line.0 = line.0.saturating_sub(1);
            }
            let (due, rest): (Vec<_>, Vec<_>) = scheduled.drain(..).partition(|line| line.0 == 0);
            *scheduled = rest;
            due
        });
        for (_, machine, left) in due {
            self.lefts.push((machine, left));
        }
    }

    /// Puts a toast and an attach end where the connections would, for a test of whoever drains them.
    pub(crate) fn announce(&mut self, machine: usize, toast: Toast, left: Left) {
        self.toasts.push((machine, toast));
        self.lefts.push((machine, left));
    }

    /// How many toasts and attach ends are waiting to be taken.
    pub(crate) fn waiting(&self) -> (usize, usize) {
        (self.toasts.len(), self.lefts.len())
    }

    /// Whether this follower has said that it can never take the keeper lock.
    fn cannot_keep_was_said(&self) -> bool {
        matches!(&self.role, Role::Follower { cannot_keep, .. } if cannot_keep.warned)
    }
}

/// A machine is live to the keeper while it is connected and heard from within [`files::LIVE_WITHIN_MS`].
fn keeper_sees_live(slot: &Slot, now_ms: u64) -> bool {
    slot.connected
        && slot
            .view
            .last_seen
            .is_some_and(|seen| now_ms.abs_diff(seen) <= files::LIVE_WITHIN_MS)
}

impl Drop for Machines {
    fn drop(&mut self) {
        // Every connection is told to end first, so that they end at the same time and not one after another.
        if let Role::Keeper { handles, .. } = &self.role {
            for handle in handles {
                handle.request_stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    #[test]
    fn a_follower_that_cannot_ever_take_the_lock_is_noticed_once_after_three_tries() {
        let mut cannot = CannotKeep::default();
        // Nobody holds the lock and every try fails: the third says it, and only the third.
        let said: Vec<bool> = (0..6).map(|_| cannot.after_a_failed_try(false)).collect();
        assert_eq!(said, [false, false, true, false, false, false]);
    }

    #[test]
    fn a_follower_polling_a_lock_file_that_is_a_folder_reaches_the_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(tmp.path().join("config"), tmp.path().join("state"));
        std::fs::create_dir_all(paths.keeper_lock_file()).unwrap();
        let ctx = ConnCtx {
            // A follower never starts a connection: no ssh is ever run.
            ssh: super::super::ssh::Ssh {
                bin: tmp.path().join("no-ssh"),
            },
            paths,
            holder: "mac-1".to_string(),
            timing: conn::Timing::default(),
            local_server: None,
        };
        let machine = Machine {
            name: "studio".to_string(),
            ssh: "studio".to_string(),
            taw: None,
        };
        let mut machines = Machines::follow(ctx, vec![machine]);
        // Each poll is a second later than the one before, so each makes one try at the lock.
        let start = Instant::now();
        let at = |second: u64, machines: &mut Machines| {
            machines.poll(start + Duration::from_secs(second));
        };
        at(1, &mut machines);
        at(2, &mut machines);
        // Positive half: two tries were made, and the warning is not there yet.
        assert!(!machines.cannot_keep_was_said());
        at(3, &mut machines);
        assert!(machines.cannot_keep_was_said());
        for second in 4..8 {
            at(second, &mut machines);
        }
        // What is read here is a flag, so this says that more tries leave it set; that the words are said
        // once and not at each try is the unit test of `CannotKeep` below (a count would need a field of
        // production code for the sake of one test).
        assert!(machines.cannot_keep_was_said());
    }

    fn paths_ctx(tmp: &std::path::Path) -> ConnCtx {
        ConnCtx {
            // A connection that cannot start its ssh: no ssh is ever run.
            ssh: super::super::ssh::Ssh {
                bin: tmp.join("no-ssh"),
            },
            paths: Paths::rooted_at(tmp.join("config"), tmp.join("state")),
            holder: "mac-1".to_string(),
            timing: conn::Timing::default(),
            local_server: None,
        }
    }

    fn named(name: &str) -> Machine {
        Machine {
            name: name.to_string(),
            ssh: name.to_string(),
            taw: None,
        }
    }

    /// Who can say which machines are live: the keeper (one flag for each machine, none of them live while
    /// no connection has been made), and nobody else: a follower and an app with no machines say `None`.
    #[test]
    fn only_the_keeper_says_which_machines_are_live() {
        let tmp = tempfile::tempdir().unwrap();
        let machines = || vec![named("studio"), named("pi")];

        let keeper = Machines::start(paths_ctx(tmp.path()), machines());
        assert_eq!(keeper.live_flags(), Some(vec![false, false]));

        // The lock is the first one's: this one follows, and a follower cannot tell.
        let follower = Machines::start(paths_ctx(tmp.path()), machines());
        assert_eq!(follower.live_flags(), None);
        let idle = Machines::start(paths_ctx(tmp.path()), Vec::new());
        assert_eq!(idle.live_flags(), None);
    }

    /// A machine is live to the keeper while it is connected and was heard from within the window, and not
    /// otherwise: connected and silent for longer, or heard from and not connected.
    #[test]
    fn a_machine_is_live_to_the_keeper_while_it_is_connected_and_heard_from() {
        let now = 1_000_000_u64;
        let mut slot = Slot::from_file("studio", None, now);
        slot.view.last_seen = Some(now - 1_000);
        assert!(!keeper_sees_live(&slot, now), "heard from, not connected");
        slot.connected = true;
        assert!(keeper_sees_live(&slot, now), "connected and heard from");
        slot.view.last_seen = Some(now - files::LIVE_WITHIN_MS - 1);
        assert!(!keeper_sees_live(&slot, now), "connected and silent");
        slot.view.last_seen = None;
        assert!(!keeper_sees_live(&slot, now), "connected, never heard");
    }

    /// Whether this app is the keeper and holds its holder's lock.
    fn holds_its_holder_lock(machines: &Machines) -> bool {
        matches!(&machines.role, Role::Keeper { alive: Some(_), .. })
    }

    /// A keeper whose holder's lock a feed of this machine's own server took first (`[machines]` lists this
    /// machine) takes it at its next poll once that feed has let it go: before, it never tried again, and an app
    /// started after it ended waited out the lease.
    #[test]
    fn a_keeper_takes_its_holder_lock_once_a_feed_that_had_it_lets_it_go() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = paths_ctx(tmp.path());
        let feed = files::take_holder_lock(&ctx.paths, &ctx.holder).expect("the feed's lock");
        let mut keeper = Machines::start(paths_ctx(tmp.path()), vec![named("studio")]);
        assert!(keeper.live_flags().is_some(), "this app is the keeper");
        keeper.poll(Instant::now());
        assert!(
            !holds_its_holder_lock(&keeper),
            "the positive half: the feed has the lock"
        );
        drop(feed);
        // The lock goes with the last descriptor of the open file, which a process another test is starting may
        // hold for the moments between its fork and its exec: polled until it is taken, not at one instant.
        let until = Instant::now() + Duration::from_secs(20);
        while !holds_its_holder_lock(&keeper) {
            assert!(Instant::now() < until, "the keeper never took its lock");
            keeper.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            files::take_holder_lock(&ctx.paths, &ctx.holder).is_none(),
            "nobody else can take it now"
        );
    }

    #[test]
    fn a_lock_another_app_holds_is_not_taken_for_one_that_cannot_be_taken() {
        let mut cannot = CannotKeep::default();
        for _ in 0..20 {
            assert!(!cannot.after_a_failed_try(true));
        }
        // And a holder that comes between two failures starts the count again.
        assert!(!cannot.after_a_failed_try(false));
        assert!(!cannot.after_a_failed_try(false));
        assert!(!cannot.after_a_failed_try(true));
        assert!(!cannot.after_a_failed_try(false));
        assert!(!cannot.after_a_failed_try(false));
        assert!(cannot.after_a_failed_try(false));
    }
}
