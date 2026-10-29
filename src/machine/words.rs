//! Every sentence about a machine that a person reads, in one place: the header's words, the refusals
//! and the toasts, so that the code that says them, the tests and the docs quote one source.
//!
//! Ages come in two styles, as the design writes each: the header's (`12s`, `2m`, `3h`, `2d`) and the
//! actions' (`12 s`, `2 min`, `3 h`, `2 d`). [`classify`] is how a connection that ended, or never
//! began, is told apart: the same for `taw machine check` and the app.

use super::wire::Hello;

/// An age, in the header's style: `12s`, `2m`, `3h`, `2d`, each rounded down.
pub fn header_age(secs: u64) -> String {
    let (n, unit) = age_parts(secs, ["s", "m", "h", "d"]);
    format!("{n}{unit}")
}

/// An age, in the style of the actions' sentences: `12 s`, `2 min`, `3 h`, `2 d`.
pub fn action_age(secs: u64) -> String {
    let (n, unit) = age_parts(secs, ["s", "min", "h", "d"]);
    format!("{n} {unit}")
}

fn age_parts(secs: u64, units: [&str; 4]) -> (u64, String) {
    let (n, unit) = match secs {
        0..=59 => (secs, units[0]),
        60..=3599 => (secs / 60, units[1]),
        3600..=86_399 => (secs / 3600, units[2]),
        _ => (secs / 86_400, units[3]),
    };
    (n, unit.to_string())
}

/// What `taw machine check` says of a machine that answered: the version of the taw there.
pub fn connected(version: &str) -> String {
    format!("connected: taw {version}")
}

/// What `taw machine check` says of a machine that did not answer: one try, so no last seen and no retry.
pub fn not_answering_once() -> String {
    "not answering".to_string()
}

/// What `taw machine check` adds after the words of an outcome that waits for you to act.
pub fn needs_you_hint() -> String {
    " (README, \"Remote machines\"); it then connects by itself".to_string()
}

pub fn no_taw() -> String {
    "no taw there: install it as you did here".to_string()
}

/// [`no_taw`] for a header that has little room: what is wrong and what to do, nothing more.
pub fn no_taw_short() -> String {
    "no taw there: install it".to_string()
}

/// `version` is what the machine's `taw --version` said; empty when it did not say.
pub fn no_machines(version: &str) -> String {
    if version.is_empty() {
        "runs a taw that has no machines: update it there".to_string()
    } else {
        format!("runs taw {version}, which has no machines: update it there")
    }
}

/// [`no_machines`] for a header that has little room, whatever version it runs.
pub fn no_machines_short() -> String {
    "old taw there: update it there".to_string()
}

pub fn newer(version: &str, ours: &str) -> String {
    format!("runs taw {version}, which no longer speaks to this one ({ours}): update taw here")
}

/// [`newer`] for a header that has little room: the version stays out, what to do stays in.
pub fn newer_short() -> String {
    "newer taw there: update taw here".to_string()
}

pub fn login() -> String {
    "ssh could not log in without asking: add your key to ssh-agent (ssh-add)".to_string()
}

/// [`login`] for a header that has little room.
pub fn login_short() -> String {
    "ssh could not log in: run ssh-add".to_string()
}

/// `dest` is what is typed after `ssh`, as `[machines]` names it.
pub fn host_key(dest: &str) -> String {
    format!("its host key is not known here: run ssh {dest} once in a terminal and accept it")
}

/// [`host_key`] for a header that has little room; `dest` is the same.
pub fn host_key_short(dest: &str) -> String {
    format!("unknown host key: run ssh {dest}, accept it")
}

/// `seen` is how many seconds ago the machine was last heard from, `retry` in how many seconds the
/// connection is tried again.
pub fn not_answering(seen: Option<u64>, retry: u64) -> String {
    match seen {
        Some(seen) => format!(
            "not answering, last seen {} ago, trying again in {}",
            header_age(seen),
            header_age(retry)
        ),
        None => format!("not answering, trying again in {}", header_age(retry)),
    }
}

pub fn garbage(name: &str) -> String {
    format!("{name} {}", garbage_header())
}

/// What the header says after the machine's name, which it has already written. It has no action to
/// keep, so it is its own short form.
pub fn garbage_header() -> String {
    "sent something taw does not read".to_string()
}

/// What a header says of a machine that is being connected.
const CONNECTING: &str = "connecting";

/// `seen` is how many seconds ago a state was last received, when there is an old one.
pub fn connecting(seen: Option<u64>) -> String {
    match seen {
        Some(seen) => format!("{CONNECTING}, last seen {} ago", header_age(seen)),
        None => CONNECTING.to_string(),
    }
}

/// Whether `header_words` are those of a machine that is being connected ([`connecting`]), as opposed
/// to one that did not answer: the header's dot is yellow for the first and red for the second.
pub fn is_connecting(header_words: &str) -> bool {
    header_words.starts_with(CONNECTING)
}

/// What a machine's header says in the popup while no app keeps the connections: the popup lists another
/// machine only through a running app, and `taw app` typed inside tmux opens the popup, so the app has to be
/// run outside tmux. It is its own short form.
pub const NO_KEEPER: &str = "not connected: run taw app outside tmux";

/// What the popup's hint line says `Enter` does while the cursor is on a row of the connected machine `name`: the
/// app takes the terminal for the trip, and `back`, the key that ends the app at once
/// ([`crate::tui::keys::leave_key`]: `Q`, or `Ctrl-C`), brings the person back to their tmux. After `Enter`, in place
/// of `go`.
pub fn enter_goes_to(name: &str, back: &str) -> String {
    format!("go to {name} ({back} there brings you back)")
}

/// [`enter_goes_to`] when the hint line has no room for the rest: the machine alone.
pub fn enter_goes_to_short(name: &str) -> String {
    format!("go to {name}")
}

/// The same in a few more words, the line of `?` under the one for Enter (while machines are listed, in the popup),
/// and the page's, with the same key `back`.
pub fn enter_on_machine_help(back: &str) -> String {
    format!(
        "on another machine's row it hands this terminal to the app; {back} there brings you back"
    )
}

pub fn this_server(name: &str) -> String {
    format!("{name} is this tmux server")
}

pub fn not_connected(name: &str, ago_secs: u64) -> String {
    format!(
        "{name} is not connected (last seen {} ago)",
        action_age(ago_secs)
    )
}

/// What the preview says in place of a screen for a row of a machine that is away.
pub fn no_preview(name: &str) -> String {
    format!("no preview: {name} is not connected")
}

/// What the preview's title says of a capture that is not fresh, in the header's age style.
pub fn capture_age(secs: u64) -> String {
    format!("({} ago)", header_age(secs))
}

/// What the top line of the app and the popup's bar say of the machines that are away while no header
/// of the list names them (a flat list draws no headers; a filter or a view can leave a machine
/// without a header): `studio, pi not connected`.
pub fn away_names(names: &[String]) -> String {
    format!("{} not connected", names.join(", "))
}

pub fn dropped_toast(name: &str) -> String {
    format!("{name} dropped off (last seen just now)")
}

/// What the popup says when tmux does not tell which server it is, so the doorway cannot pin the app to it.
pub const NO_SERVER_PATH: &str = "could not tell which tmux server this is";

/// What a landing says that has nowhere to go: a row kind that does not land, or one a machine's row does not
/// make yet. The same words on this Mac and on a machine.
pub const NOTHING_TO_GO_IN_TO: &str = "nothing to go in to";

/// What `taw app --go` says of a target that is none of its four forms: `last`, a pane of this Mac (`%7`), a
/// machine and one of its ids (`studio:%16`) or a machine and `new` (`studio:new`).
pub const GO_USAGE: &str = "--go takes last, a pane of this machine as in %7, a machine and one of its ids as in studio:%16, or a machine and new as in studio:new";

/// The word after a machine's name in `--go studio:new`, and the target the app's attach carries for `taw machine
/// attach --new`: a new tmux session on a machine that has none. It is no tmux id, so it cannot be taken for one.
pub const NEW_ID: &str = "new";

/// What a machine's tmux shows on the status line of a client the app attached, for [`BACK_HINT_MS`]: how to get
/// back to the taw list. tmux fills in the machine's own prefix (`C-b`, as tmux names it) and the first key of the
/// plugin's `@agent-wizard` option (`A` by default) when it draws the message, so it is right on that machine
/// whatever it has set, and for a session that has just been started. Where the plugin is not loaded in that tmux
/// (`@agent-wizard` unset: only the release binary, a server started before the plugin, a tmux.conf without its
/// line) there is no such key, and the words name the prefix and `d`, the detach that brings the list back too.
/// The far tmux still decides between the two, when it draws the message.
pub const BACK_HINT: &str =
    "#{?@agent-wizard,#{prefix} + #{s/ .*//r:@agent-wizard},#{prefix} + d}: back to the taw list";

/// How long [`BACK_HINT`] stays on the status line, in milliseconds as tmux's `display-message -d` takes it.
pub const BACK_HINT_MS: &str = "4000";

/// The row of a connected machine that has no tmux session, the one place to press Enter on.
pub const NO_SESSION_ROW: &str = "no tmux session here: Enter starts one";

/// What the app says when the last session of a machine ended and the machine has none now.
pub fn no_session_now(name: &str) -> String {
    format!("{name} has no tmux session now: Enter on its row starts one")
}

/// What `taw app --go` says of a pane or a target that tmux no longer has: `%7 is gone`.
pub fn gone(target: &str) -> String {
    format!("{target} is gone")
}

/// What going in says of a machine that is not one of `[machines]` (its row went when the config changed, or an id
/// was given by hand).
pub fn no_such_machine(name: &str) -> String {
    format!("no machine called {name}")
}

/// What an attach that ended says, in one place: the words the far taw printed on the terminal are gone when the
/// screen is drawn again, so this is all the person gets. `code` is the status ssh exited with and `signal` the
/// signal that ended it when it had no status. A clean end says nothing.
///
/// 255 is ssh's own failure (it could not reach the machine, or lost it); so is an ssh that a signal ended, which
/// is what a connection that went does to it. 2 is the far `taw` refusing: the target is gone, or the taw there
/// is too old to know `machine attach` (the plain words cover both, the terminal's are gone).
pub fn attach_ended(name: &str, code: Option<i32>, signal: Option<i32>) -> Option<String> {
    match (code, signal) {
        (Some(0), _) => None,
        (Some(255), _) => Some(format!("{name} did not answer (ssh ended with status 255)")),
        (Some(2), _) => Some(format!(
            "{name} refused the attach: the pane or session may be gone, or the taw there is older and does not know machine attach"
        )),
        (Some(other), _) => Some(format!("the attach to {name} ended with status {other}")),
        (None, Some(signal)) => Some(format!(
            "{name} did not answer (ssh was ended by signal {signal})"
        )),
        (None, None) => Some(format!("{name} did not answer (ssh was stopped)")),
    }
}

/// What the app asks of a person after an attach that failed, while the far side's own words are still on the
/// terminal: one line under them, then it waits.
pub const ATTACH_HELD: &str = "Press Enter to go back to the list.";

/// The line the app prints under the words of a failed attach: `failure` is [`attach_ended`]'s words.
pub fn attach_held(failure: &str) -> String {
    format!("taw: {failure}. {ATTACH_HELD}")
}

/// What opening a closed worktree on `name` says when the machine could not do it: `why` is its own message.
pub fn worktree_open_failed(name: &str, why: &str) -> String {
    format!("{name} could not open the worktree: {why}")
}

/// What it says when the machine's reply to that open was not one taw reads.
pub fn worktree_open_unreadable(name: &str) -> String {
    format!("{name} answered the worktree open with something taw does not read")
}

/// What it says when `machine` made a worktree and the app did not go in (the cursor moved, or something else
/// was open when it answered): the session is there, and Enter on its row goes in.
pub fn worktree_made_not_opened(machine: &str) -> String {
    format!("worktree made on {machine}: Enter on its row goes in")
}

/// What it says when `machine` removed a worktree: the popup's words, with the machine named. `branch` is the
/// worktree's branch when the machine's answer said it.
pub fn worktree_removed(machine: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("removed worktree {branch} on {machine}"),
        None => format!("removed worktree on {machine}"),
    }
}

/// What the list says while `name` opens a closed worktree, before the session it makes is attached.
pub fn opening_worktree(name: &str) -> String {
    format!("opening worktree on {name}")
}

/// What it says when the machine took longer than `secs` seconds to open a worktree and ssh was ended.
pub fn worktree_open_timed_out(name: &str, secs: u64) -> String {
    format!("{name} did not open the worktree within {secs} s; ssh was ended")
}

/// What it says when ssh itself could not be run for `name`.
pub fn ssh_not_run(name: &str, why: &str) -> String {
    format!("{name}: ssh could not be run ({why})")
}

pub fn read_only(name: &str) -> String {
    format!("{name}'s rows are acted on from the app: Enter goes there")
}

/// `read_only` without its Enter clause, for the Enter just pressed on a live machine's closed worktree in the
/// popup: it is not a doorway, and the key that was pressed is not told where it goes.
pub fn acted_on_from_the_app(name: &str) -> String {
    format!("{name}'s rows are acted on from the app")
}

pub fn waiting(name: &str) -> String {
    format!("waiting for {name}")
}

/// What the app says when a second action is asked of a machine that is still answering the first.
pub fn still_answering(machine: &str) -> String {
    format!("{machine} is still answering the last action")
}

/// What the app says when the machine answers that the row it was asked about is not there.
pub fn row_gone(machine: &str) -> String {
    format!("{machine}: that row is gone")
}

/// What the app says when a machine has not answered an action within its bound.
pub fn timed_out(machine: &str, secs: u64) -> String {
    format!("no answer from {machine} in {secs} s; it may still have run")
}

/// A target in a confirmation, named with the machine it is on: `studio · garden:1`.
pub fn on_machine(machine: &str, target: &str) -> String {
    format!("{machine} \u{b7} {target}")
}

/// The title of the prompt `n` opens on a row of `machine`: the session is made there.
pub fn new_session_prompt(machine: &str) -> String {
    format!("new session on {machine}: a folder or a zoxide term")
}

/// What the app says when `machine` made a session for `dir` (`created`), or already had one there.
pub fn session_made(machine: &str, name: &str, dir: &str, created: bool) -> String {
    if created {
        format!("made session {name} on {machine}: {dir}")
    } else {
        format!("session {name} is already on {machine}: {dir}")
    }
}

/// What the app says when the taw on `machine` does not know `open --no-switch`.
pub fn update_taw_to_make_sessions(machine: &str) -> String {
    format!("update taw on {machine} to make sessions there from here")
}

/// What the app says when `machine` answered that it made the session but the answer holds no name to go in by.
pub fn session_made_unreadable(machine: &str) -> String {
    format!("{machine} answered, but the answer names no session")
}

/// What the app says of an action whose target is not an id it can send to a machine.
pub fn no_id_to_send(machine: &str) -> String {
    format!("{machine}: that row has no id taw can send")
}

/// What the app says when a machine's answer comes after the list moved on: the answer asks something
/// of the row it was for, so it is not asked of another.
pub fn answered_late(machine: &str) -> String {
    format!("{machine} answered after the list changed; look again before trying once more")
}

/// The question `x` on a session with worktree sessions open asks, on this machine and on a machine's
/// session alike: `lead`, which says what would close, then how many worktree sessions are open and which
/// (three named at most), and whether they go too. `names` is the open sessions' names, at least one.
pub fn group_close_named(lead: &str, names: &[&str]) -> String {
    let shown = match names {
        [a, b, c, rest @ ..] if !rest.is_empty() => {
            format!("{a}, {b}, {c} and {} more", rest.len())
        }
        all => all.join(", "),
    };
    let plural = if names.len() == 1 { "" } else { "s" };
    format!(
        "{lead} It has {} worktree session{plural} open ({shown}). Close them too?",
        names.len()
    )
}

/// The question asked when a machine says closing a session closes worktree sessions too and the list
/// had not caught up with the machine, so the app does not know which are open.
pub fn group_close_unseen(lead: &str) -> String {
    format!("{lead} It has worktree sessions open. Close them too?")
}

pub fn no_answer(name: &str) -> String {
    format!("{name} did not answer; look again before trying once more")
}

/// What it says of an error the machine's taw reported for an action: its own words, with the machine named.
pub fn refused(machine: &str, message: &str) -> String {
    if message.trim().is_empty() {
        return format!("{machine} answered with an error and no message");
    }
    format!("{machine}: {message}")
}

/// `open` found no folder for `term` over there and said nothing more: the words the far taw uses when it does
/// say, as the app's `n` shows them.
pub fn no_folder(machine: &str, term: &str) -> String {
    format!("{machine}: no directory matches {term}")
}

/// `label` is the hook's own name (`pre-close hook` or `pre-worktree-remove hook`).
pub fn hook_refused(machine: &str, label: &str, why: &str) -> String {
    format!("{label} on {machine}: {why}")
}

/// What was heard from the far end before it ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Heard<'a> {
    /// Nothing on standard output.
    Nothing,
    /// A first line that is a hello.
    Hello(&'a Hello),
    /// A first line that is anything else.
    Other,
}

/// How a try at a machine came out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A hello of protocol 1, from a taw of this version.
    Connected { taw: String },
    /// No `taw` there (the shell said 127).
    NoTaw,
    /// A taw with no `machine` verb (exit 2): the caller asks it for its version.
    NoMachines,
    /// A taw that speaks another protocol.
    Newer { taw: String },
    /// ssh could not log in without asking.
    Login,
    /// The host key is not known here.
    HostKey,
    /// Anything else that was ssh's own failure, or no word at all.
    NotAnswering,
    /// It answered with something that is not a hello.
    Garbage,
}

impl Outcome {
    /// The five outcomes that wait for the user to do something: after it, the connection is made
    /// without being asked again.
    pub fn needs_you(&self) -> bool {
        matches!(
            self,
            Outcome::NoTaw
                | Outcome::NoMachines
                | Outcome::Newer { .. }
                | Outcome::Login
                | Outcome::HostKey
        )
    }
}

/// How the try came out, from the exit status (`None` when it was killed or did not end), the last line
/// the far end said on standard error and the first line it said on standard output.
///
/// A hello decides first: it is a taw that speaks. Without one, the exit status says what the shell or
/// ssh said (127 no taw, 2 a taw that does not know `machine`, 255 ssh's own failures by what it ended
/// on), and a line that is not a hello is what is left.
pub fn classify(exit: Option<i32>, stderr_last: &str, heard: Heard) -> Outcome {
    if let Heard::Hello(hello) = heard {
        return match hello.protocol {
            1 => Outcome::Connected {
                taw: hello.taw.clone(),
            },
            // A hello with no `protocol` reads as 0: it is not from a taw that speaks this protocol, and
            // `newer` would send the user to update the wrong side, so it counts as not a feed.
            0 => Outcome::Garbage,
            _ => Outcome::Newer {
                taw: hello.taw.clone(),
            },
        };
    }
    match exit {
        Some(127) => Outcome::NoTaw,
        Some(2) => Outcome::NoMachines,
        Some(255) if stderr_last.contains("Permission denied") => Outcome::Login,
        Some(255) if stderr_last.contains("Host key verification failed") => Outcome::HostKey,
        Some(255) => Outcome::NotAnswering,
        _ if heard == Heard::Other => Outcome::Garbage,
        _ => Outcome::NotAnswering,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_in_the_headers_style() {
        let ages: Vec<String> = [
            0, 12, 59, 60, 119, 120, 3599, 3600, 10_800, 86_399, 86_400, 172_800,
        ]
        .iter()
        .map(|s| header_age(*s))
        .collect();
        assert_eq!(
            ages,
            [
                "0s", "12s", "59s", "1m", "1m", "2m", "59m", "1h", "3h", "23h", "1d", "2d"
            ]
        );
    }

    #[test]
    fn ages_in_the_actions_style() {
        let ages: Vec<String> = [12, 120, 10_800, 172_800]
            .iter()
            .map(|s| action_age(*s))
            .collect();
        assert_eq!(ages, ["12 s", "2 min", "3 h", "2 d"]);
    }

    #[test]
    fn every_sentence_reads_as_the_design_writes_it() {
        assert_eq!(connected("0.4.0"), "connected: taw 0.4.0");
        assert_eq!(not_answering_once(), "not answering");
        assert_eq!(
            needs_you_hint(),
            " (README, \"Remote machines\"); it then connects by itself"
        );
        assert_eq!(no_taw(), "no taw there: install it as you did here");
        assert_eq!(no_taw_short(), "no taw there: install it");
        assert_eq!(no_machines_short(), "old taw there: update it there");
        assert_eq!(newer_short(), "newer taw there: update taw here");
        assert_eq!(login_short(), "ssh could not log in: run ssh-add");
        assert_eq!(
            host_key_short("studio.local"),
            "unknown host key: run ssh studio.local, accept it"
        );
        assert_eq!(garbage_header(), "sent something taw does not read");
        assert_eq!(NO_KEEPER, "not connected: run taw app outside tmux");
        assert_eq!(
            no_machines("0.0.9"),
            "runs taw 0.0.9, which has no machines: update it there"
        );
        assert_eq!(
            no_machines(""),
            "runs a taw that has no machines: update it there"
        );
        assert_eq!(
            newer("0.4.0", "0.3.0"),
            "runs taw 0.4.0, which no longer speaks to this one (0.3.0): update taw here"
        );
        assert_eq!(
            login(),
            "ssh could not log in without asking: add your key to ssh-agent (ssh-add)"
        );
        assert_eq!(
            host_key("studio.local"),
            "its host key is not known here: run ssh studio.local once in a terminal and accept it"
        );
        assert_eq!(
            not_answering(Some(120), 30),
            "not answering, last seen 2m ago, trying again in 30s"
        );
        assert_eq!(
            not_answering(None, 30),
            "not answering, trying again in 30s"
        );
        assert_eq!(garbage("studio"), "studio sent something taw does not read");
        assert_eq!(garbage_header(), "sent something taw does not read");
        assert_eq!(connecting(None), "connecting");
        assert_eq!(connecting(Some(10_800)), "connecting, last seen 3h ago");
        assert_eq!(this_server("pi"), "pi is this tmux server");
        assert_eq!(
            not_connected("pi", 120),
            "pi is not connected (last seen 2 min ago)"
        );
        assert_eq!(
            away_names(&["studio".to_string(), "pi".to_string()]),
            "studio, pi not connected"
        );
        assert_eq!(away_names(&["pi".to_string()]), "pi not connected");
        assert_eq!(dropped_toast("pi"), "pi dropped off (last seen just now)");
        assert_eq!(no_preview("pi"), "no preview: pi is not connected");
        assert_eq!(capture_age(5), "(5s ago)");
        assert_eq!(capture_age(125), "(2m ago)");
        assert_eq!(
            read_only("pi"),
            "pi's rows are acted on from the app: Enter goes there"
        );
        assert_eq!(
            acted_on_from_the_app("pi"),
            "pi's rows are acted on from the app"
        );
        assert_eq!(no_such_machine("pi"), "no machine called pi");
        assert_eq!(NOTHING_TO_GO_IN_TO, "nothing to go in to");
        assert_eq!(
            GO_USAGE,
            "--go takes last, a pane of this machine as in %7, a machine and one of its ids as in studio:%16, or a machine and new as in studio:new"
        );
        assert_eq!(
            still_answering("studio"),
            "studio is still answering the last action"
        );
        assert_eq!(row_gone("studio"), "studio: that row is gone");
        assert_eq!(
            timed_out("studio", 20),
            "no answer from studio in 20 s; it may still have run"
        );
        assert_eq!(on_machine("studio", "garden:1"), "studio \u{b7} garden:1");
        assert_eq!(
            no_id_to_send("studio"),
            "studio: that row has no id taw can send"
        );
        assert_eq!(
            answered_late("studio"),
            "studio answered after the list changed; look again before trying once more"
        );
        assert_eq!(
            group_close_unseen("kill-session studio \u{b7} garden?"),
            "kill-session studio \u{b7} garden? It has worktree sessions open. Close them too?"
        );
        assert_eq!(gone("%7"), "%7 is gone");
        assert_eq!(
            worktree_open_timed_out("pi", 20),
            "pi did not open the worktree within 20 s; ssh was ended"
        );
        assert_eq!(opening_worktree("pi"), "opening worktree on pi");
        assert_eq!(
            worktree_made_not_opened("studio"),
            "worktree made on studio: Enter on its row goes in"
        );
        assert_eq!(
            worktree_removed("studio", Some("wip")),
            "removed worktree wip on studio"
        );
        assert_eq!(
            worktree_removed("studio", None),
            "removed worktree on studio"
        );
        assert_eq!(
            refused("pi", "pane '%9' not found"),
            "pi: pane '%9' not found"
        );
        assert_eq!(waiting("pi"), "waiting for pi");
        assert_eq!(
            no_answer("pi"),
            "pi did not answer; look again before trying once more"
        );
        assert_eq!(
            hook_refused("pi", "pre-close hook", "not now"),
            "pre-close hook on pi: not now"
        );
    }

    fn hello(protocol: u32) -> Hello {
        Hello {
            taw: "0.1.0".into(),
            protocol,
            ..Hello::default()
        }
    }

    #[test]
    fn a_hello_of_protocol_one_is_connected_and_another_protocol_is_newer() {
        let ok = hello(1);
        assert_eq!(
            classify(Some(0), "", Heard::Hello(&ok)),
            Outcome::Connected {
                taw: "0.1.0".into()
            }
        );
        let other = hello(2);
        assert_eq!(
            classify(Some(0), "", Heard::Hello(&other)),
            Outcome::Newer {
                taw: "0.1.0".into()
            }
        );
        assert_eq!(
            classify(Some(0), "", Heard::Hello(&hello(0))),
            Outcome::Garbage
        );
    }

    #[test]
    fn the_exit_status_and_the_last_words_tell_the_failures_apart() {
        let c = |exit, said, heard| classify(exit, said, heard);
        assert_eq!(
            c(Some(127), "sh: taw: command not found", Heard::Nothing),
            Outcome::NoTaw
        );
        assert_eq!(
            c(Some(2), "error: unrecognized subcommand", Heard::Nothing),
            Outcome::NoMachines
        );
        assert_eq!(
            c(
                Some(255),
                "studio: Permission denied (publickey).",
                Heard::Nothing
            ),
            Outcome::Login
        );
        assert_eq!(
            c(Some(255), "Host key verification failed.", Heard::Nothing),
            Outcome::HostKey
        );
        assert_eq!(
            c(
                Some(255),
                "ssh: connect to host studio port 22: Connection refused",
                Heard::Nothing
            ),
            Outcome::NotAnswering
        );
        assert_eq!(c(Some(255), "", Heard::Nothing), Outcome::NotAnswering);
        assert_eq!(c(None, "", Heard::Nothing), Outcome::NotAnswering);
        assert_eq!(c(None, "", Heard::Other), Outcome::Garbage);
        assert_eq!(c(Some(0), "", Heard::Other), Outcome::Garbage);
        // ssh's own failure beats what was said before it.
        assert_eq!(c(Some(255), "", Heard::Other), Outcome::NotAnswering);
    }

    #[test]
    fn five_outcomes_wait_for_the_user() {
        let wait = |o: Outcome| o.needs_you();
        assert!(wait(Outcome::NoTaw) && wait(Outcome::NoMachines) && wait(Outcome::Login));
        assert!(wait(Outcome::HostKey) && wait(Outcome::Newer { taw: String::new() }));
        assert!(!wait(Outcome::NotAnswering) && !wait(Outcome::Garbage));
        assert!(!wait(Outcome::Connected { taw: String::new() }));
    }

    /// Each way an attach ends, in the words the person reads: ssh's own failure, the far taw refusing, any other
    /// status, a signal, and a clean end that says nothing.
    #[test]
    fn an_attach_that_ended_is_told_by_its_status() {
        assert_eq!(attach_ended("studio", Some(0), None), None);
        let said = attach_ended("studio", Some(255), None).unwrap();
        assert!(said.starts_with("studio did not answer"), "{said}");
        assert!(said.contains("255"), "{said}");
        let said = attach_ended("studio", Some(2), None).unwrap();
        assert!(said.starts_with("studio refused the attach"), "{said}");
        assert!(said.contains("older"), "{said}");
        assert_eq!(
            attach_ended("studio", Some(7), None).as_deref(),
            Some("the attach to studio ended with status 7")
        );
        let said = attach_ended("studio", None, Some(9)).unwrap();
        assert!(said.starts_with("studio did not answer"), "{said}");
        assert!(said.contains("signal 9"), "{said}");
        assert!(
            attach_ended("studio", None, None)
                .unwrap()
                .contains("did not answer")
        );
    }
}
