//! Waiting for a pane's shell to come to its prompt before anything is typed into it.
//!
//! What a pane's shell is doing cannot be seen from outside, and what is typed at a shell that
//! is still starting goes to whatever reads the terminal first. A shell that starts mise can
//! stop at its question "Trust them? Yes No All": a restore that typed `claude --resume <id>`
//! into a pane it had just made would have the `a` in `claude` answer `All`, trusting a folder
//! nobody had said anything about. Any other question a shell's startup asks takes the keystrokes the same way.
//!
//! So the pane is looked at first. Its shell is at its prompt when the watcher's own test says
//! so ([`crate::watch::pane_shell_is_idle`], herdr's: the terminal's foreground group is led by
//! the pane's process, alone, and it is a shell), and has stayed that way with the screen
//! unchanged for [`SETTLE`], because the same test also passes for a moment between two
//! commands of a shell's rc file. A question asked by a program the shell runs fails the test
//! for as long as it is up, which is the case this exists for. A shell that has not come to its
//! prompt within [`BOUND`] is given up on, and nothing is typed.
//!
//! A shell that has drawn nothing yet is not at its prompt either, whatever the test says: a
//! shell that has only just started passes it too, and a startup that works through its rc
//! file without a program or a word of output for as long as the settle would be taken for a
//! prompt, with the question it has still to ask coming after it.
//!
//! A pane that is already at its prompt costs one look. tmux keeps the time of a window's last
//! output, to the second, and a window that has been quiet for the settle and a second more
//! has nothing a second look could find moving.
//!
//! A pane that takes no input is given up on at once ([`Outcome::NoInput`]): tmux gives what is
//! sent to a pane in a mode (copy mode, the clock, a chooser) to the mode, which drops it with
//! nobody attached and runs it as the mode's commands with someone, and sends nothing to a pane
//! whose input is off (`select-pane -d`). What is typed would never reach the shell, and nothing
//! would come of the wait but its whole bound. The pane is left as it is: a person in copy mode may
//! be reading the scrollback.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::tmux::{CommandFailed, Tmux};

/// How long a shell gets to come to its prompt.
pub const BOUND: Duration = Duration::from_secs(15);

/// How long the screen has to stay as it is, with the shell idle, before the prompt is believed.
pub const SETTLE: Duration = Duration::from_millis(300);

/// How often a pane that is not ready yet is looked at again.
pub const POLL: Duration = Duration::from_millis(50);

/// The three numbers a wait runs on.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub bound: Duration,
    pub settle: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            bound: BOUND,
            settle: SETTLE,
            poll: POLL,
        }
    }
}

/// How a wait for a pane's shell ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The shell is at its prompt: type now.
    Ready,
    /// It had not come to its prompt when the bound ran out: type nothing.
    TimedOut,
    /// The pane is gone: closed, or its shell ended.
    Gone,
    /// The pane takes no input, for this reason: type nothing.
    NoInput(NoInput),
}

/// Why a pane takes none of the keys sent to it: tmux gives them to the mode a pane is in, and sends none
/// to a pane whose input is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoInput {
    /// The pane is in a mode, as tmux names it in words (`copy mode`, `clock mode`, `tree mode`).
    Mode(String),
    /// Its input is off (`select-pane -d`).
    Off,
}

impl NoInput {
    /// Why a pane whose `#{pane_in_mode}`, `#{pane_mode}` and `#{pane_input_off}` read these takes no
    /// input, or `None` when it takes it.
    pub fn of(in_mode: &str, mode: &str, input_off: &str) -> Option<NoInput> {
        if in_mode.trim() == "1" {
            Some(NoInput::Mode(match mode.trim() {
                "" => "a mode".to_string(),
                mode => mode.replace('-', " "),
            }))
        } else if input_off.trim() == "1" {
            Some(NoInput::Off)
        } else {
            None
        }
    }

    /// What is so of `pane` (named as the caller named it): `pane w1:p3 is in copy mode`.
    pub fn of_pane(&self, pane: &str) -> String {
        match self {
            NoInput::Mode(mode) => format!("pane {pane} is in {mode}"),
            NoInput::Off => format!("pane {pane} takes no input"),
        }
    }

    /// What a verb that will not type into `pane` says: what is so of it, and what to do about it.
    pub fn refusal(&self, pane: &str) -> String {
        let what_to_do = match self {
            NoInput::Mode(_) => "leave it and try again",
            NoInput::Off => "turn its input back on and try again",
        };
        format!("{}; {what_to_do}", self.of_pane(pane))
    }

    /// What a verb says of `pane` found to take no input once `typed` (`the command`, `the prompt`) had
    /// been typed into it: the same, and that what was typed may be left on its line, which the verb does
    /// not clear, since it would have to take the pane out of its mode on the user's behalf.
    pub fn refusal_after_typing(&self, pane: &str, typed: &str) -> String {
        let what_to_do = match self {
            NoInput::Mode(_) => "leave the mode, clear the line and try again",
            NoInput::Off => "turn its input back on, clear the line and try again",
        };
        format!(
            "{}, and {typed} typed into it may be left on its line; {what_to_do}",
            self.of_pane(pane)
        )
    }
}

/// Waits for `pane`'s shell, looking at it every [`Timing::poll`], for at most [`Timing::bound`].
pub fn wait(t: &Tmux, pane: &str, timing: Timing) -> anyhow::Result<Outcome> {
    let mut waiter = Waiter::new(pane, Instant::now(), timing);
    loop {
        if let Some(outcome) = waiter.look(t)? {
            return Ok(outcome);
        }
        std::thread::sleep(timing.poll);
    }
}

/// The answer of [`at_prompt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtPrompt {
    /// The shell is at its prompt.
    Yes,
    /// It is not: a program has the terminal, or the shell has not drawn its prompt or held it still.
    No,
    /// The pane is gone: closed, or its shell ended.
    Gone,
}

/// Whether `pane`'s own shell is at its prompt now, for a caller that acts only on a pane that has one
/// and would rather do nothing than wait on a program: `taw pane clear`, which types a key at a shell
/// that is at its prompt and anywhere else leaves the screen as it is.
///
/// It is the same test as [`wait`]'s, looked at the same way: the shell is idle by the process table,
/// has drawn something, and has held its screen still for [`Timing::settle`], either by tmux's clock or
/// by two looks that far apart. What differs is a shell that is not idle. [`wait`] looks again, because
/// a shell that is still starting will come to its prompt; here the answer is no at once, because a
/// program that has the terminal now (an agent, a build, an editor) is the ordinary case, and a look
/// that waited on it would hold the caller up for the whole bound. A shell that is idle and has not
/// held still yet is looked at again, for at most [`Timing::bound`].
pub fn at_prompt(t: &Tmux, pane: &str, timing: Timing) -> anyhow::Result<AtPrompt> {
    let deadline = Instant::now() + timing.bound;
    let mut still = Still::default();
    loop {
        let now = Instant::now();
        let Some(seen) = see(t, pane, timing.settle)? else {
            return Ok(AtPrompt::Gone);
        };
        if seen.idle != Some(true) {
            return Ok(AtPrompt::No);
        }
        if still.settled(now, &seen, timing.settle) {
            return Ok(AtPrompt::Yes);
        }
        if now >= deadline {
            return Ok(AtPrompt::No);
        }
        std::thread::sleep(timing.poll);
    }
}

/// One pane's wait, for a caller that has several panes to wait on and gets on with the ones
/// that are ready while the others are not: it looks at each in turn, and a wait that is long
/// for one pane is not a wait at all for the rest.
pub struct Waiter {
    pane: String,
    deadline: Instant,
    timing: Timing,
    still: Still,
    /// What tmux named in front of the pane at the last look.
    command: String,
}

impl Waiter {
    /// The wait for `pane`'s shell, which has the bound from `from` to come to its prompt.
    pub fn new(pane: &str, from: Instant, timing: Timing) -> Waiter {
        Waiter {
            pane: pane.to_string(),
            deadline: from + timing.bound,
            timing,
            still: Still::default(),
            command: String::new(),
        }
    }

    /// Looks at the pane once: `None` while the wait goes on.
    ///
    /// A shell that is ready is ready whatever the time: the look that finds it so comes before
    /// the one that finds the bound run out, so a pane that has been at its prompt all along
    /// is never given up on for a caller that came to it late.
    ///
    /// An error is tmux failing for a reason other than the pane being gone, and the wait
    /// cannot go on.
    pub fn look(&mut self, t: &Tmux) -> anyhow::Result<Option<Outcome>> {
        let now = Instant::now();
        let Some(mut seen) = see(t, &self.pane, self.timing.settle)? else {
            return Ok(Some(Outcome::Gone));
        };
        self.command = std::mem::take(&mut seen.command);
        if let Some(why) = seen.no_input {
            return Ok(Some(Outcome::NoInput(why)));
        }
        if self.still.settled(now, &seen, self.timing.settle) {
            return Ok(Some(Outcome::Ready));
        }
        Ok((now >= self.deadline).then_some(Outcome::TimedOut))
    }

    /// The command tmux named in front of the pane at the last look (`#{pane_current_command}`): at the
    /// look that found the shell at its prompt, the shell's own. A caller that types into the pane once
    /// the wait is over sends only while it is still in front (`cli::common::send_text_if_in_front`): a
    /// program started in the pane since that look would be given what is typed.
    pub fn command(&self) -> &str {
        &self.command
    }
}

/// Whether `err` is tmux saying it has no such pane, window or session: what a command that
/// names something that was closed since fails with. tmux says it two ways: "can't find pane"
/// from a command that acts on a target, and "no such pane" from one that sets an option on it.
pub fn is_gone(err: &anyhow::Error) -> bool {
    const SAYINGS: [&str; 6] = [
        "can't find pane",
        "can't find window",
        "can't find session",
        "no such pane",
        "no such window",
        "no such session",
    ];
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<CommandFailed>())
        .any(|failed| SAYINGS.iter().any(|saying| failed.stderr.contains(saying)))
}

/// What one look at a pane found.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    /// Whether the pane's own shell is idle, as the watcher asks it: `None` when tmux, `ps` or
    /// `pgrep` could not say.
    idle: Option<bool>,
    /// Whether anything has been drawn in the pane: its cursor has left the corner it starts
    /// in, or its screen has text on it.
    drawn: bool,
    /// Whether tmux's clock says nothing has been drawn in the pane's window for the settle.
    quiet: bool,
    /// The pane's screen and cursor. Taken only when the shell is idle and the clock has not
    /// vouched for the screen already, which is when a second look has to say it is unchanged.
    screen: Option<String>,
    /// Why the pane takes no input, when it takes none.
    no_input: Option<NoInput>,
    /// What tmux names in front of the pane (`#{pane_current_command}`).
    command: String,
}

/// What a look asks tmux: the pane's own id, whether it is dead, its process, when its window
/// last drew, where the cursor is, whether it takes input, and what is in front of it, last, since
/// it is a name that can hold anything. The id is there to tell a pane from none: `display-message`
/// answers for a pane that is not there with a line of empty fields, and does not fail.
const FORMAT: &str = "#{pane_id}\t#{pane_dead}\t#{pane_pid}\t#{window_activity}\t#{cursor_x}\t\
                      #{cursor_y}\t#{pane_in_mode}\t#{pane_mode}\t#{pane_input_off}\t\
                      #{pane_current_command}";

/// Looks at `pane` once; `None` when it is gone.
fn see(t: &Tmux, pane: &str, settle: Duration) -> anyhow::Result<Option<Seen>> {
    // Read before tmux is asked, so that the quiet worked out from the answer is never longer
    // than it was.
    let asked = unix_millis(SystemTime::now());
    let line = match t.display(pane, FORMAT) {
        Ok(line) => line,
        Err(err) if is_gone(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut fields = line.splitn(10, '\t');
    if fields.next().is_none_or(str::is_empty) {
        return Ok(None);
    }
    let (dead, pid, activity) = (fields.next(), fields.next(), fields.next());
    let (x, y) = (
        fields.next().unwrap_or_default(),
        fields.next().unwrap_or_default(),
    );
    let no_input = NoInput::of(
        fields.next().unwrap_or_default(),
        fields.next().unwrap_or_default(),
        fields.next().unwrap_or_default(),
    );
    let command = fields.next().unwrap_or_default().to_string();
    if dead == Some("1") {
        return Ok(None);
    }
    let Some(pid) = pid.and_then(|pid| pid.trim().parse::<u32>().ok()) else {
        return Ok(Some(Seen {
            no_input,
            command,
            ..Seen::default()
        }));
    };
    let idle = crate::watch::pane_shell_is_idle(pid);
    // What is in front of the pane once its shell is found idle, which is then the shell's own name: a
    // caller that types into the pane sends only while it is still there. Read again after the process
    // table, since the line above was read before it: a look in which it changed in between (a program
    // the shell ran ending, an answer to a question just typed) cannot tell a shell at rest, and the
    // window's clock and the cursor it read may be from before the change.
    let (idle, command) = match idle {
        Some(true) => match t.display(pane, "#{pane_current_command}") {
            Ok(now) if now == command => (idle, now),
            Ok(now) => (None, now),
            Err(err) if is_gone(&err) => return Ok(None),
            Err(err) => return Err(err),
        },
        _ => (idle, command),
    };
    let moved = (x, y) != ("0", "0");
    let quiet = idle == Some(true)
        && moved
        && quiet_for(
            asked,
            activity.and_then(|secs| secs.trim().parse().ok()),
            settle,
        );
    let text = if idle == Some(true) && !quiet {
        match t.capture_pane(pane, false) {
            Ok(text) => Some(text),
            Err(err) if is_gone(&err) => return Ok(None),
            Err(err) => return Err(err),
        }
    } else {
        None
    };
    Ok(Some(Seen {
        idle,
        drawn: moved || text.as_deref().is_some_and(|text| !text.trim().is_empty()),
        quiet,
        screen: text.map(|text| format!("{x},{y}\n{text}")),
        no_input,
        command,
    }))
}

/// Whether a window whose last output was in the second `activity` (tmux prints whole seconds,
/// rounded down, so the output may have come up to a second later than that) has been quiet for
/// `settle` at `now_ms`. `None`, a clock tmux did not print, vouches for nothing.
fn quiet_for(now_ms: i64, activity: Option<i64>, settle: Duration) -> bool {
    let Some(activity) = activity else {
        return false;
    };
    let latest_output = activity.saturating_add(1).saturating_mul(1000);
    let settle = i64::try_from(settle.as_millis()).unwrap_or(i64::MAX);
    now_ms.saturating_sub(latest_output) >= settle
}

fn unix_millis(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// How long a pane's shell has been idle with its screen as it is.
#[derive(Debug, Default)]
struct Still {
    /// When the screen below was first seen, with the shell idle.
    since: Option<(Instant, String)>,
}

impl Still {
    /// Takes one look in, and says whether the shell can be believed to be at its prompt: idle,
    /// something drawn, and the screen unchanged for `settle`, either by tmux's clock or by two
    /// looks that far apart. A look at a shell that is not idle, or that cannot be told to be,
    /// or that has drawn nothing, starts it over.
    fn settled(&mut self, now: Instant, seen: &Seen, settle: Duration) -> bool {
        if seen.idle != Some(true) || !seen.drawn {
            self.since = None;
            return false;
        }
        if seen.quiet {
            return true;
        }
        let Some(screen) = &seen.screen else {
            self.since = None;
            return false;
        };
        match &self.since {
            Some((first, was)) if was == screen => now.duration_since(*first) >= settle,
            _ => {
                self.since = Some((now, screen.clone()));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn idle(screen: &str) -> Seen {
        Seen {
            idle: Some(true),
            drawn: true,
            quiet: false,
            screen: Some(screen.to_string()),
            no_input: None,
            command: String::new(),
        }
    }

    fn busy() -> Seen {
        Seen {
            idle: Some(false),
            ..Seen::default()
        }
    }

    #[test]
    fn the_defaults_are_a_bound_of_15_seconds_and_a_settle_of_300_ms() {
        let timing = Timing::default();
        assert_eq!(timing.bound, Duration::from_secs(15));
        assert_eq!(timing.settle, Duration::from_millis(300));
        assert_eq!(timing.poll, Duration::from_millis(50));
    }

    #[test]
    fn an_idle_shell_is_believed_once_its_screen_has_held_still_for_the_settle() {
        let base = Instant::now();
        let mut still = Still::default();
        assert!(
            !still.settled(base, &idle("$ "), SETTLE),
            "the first look only starts it"
        );
        assert!(!still.settled(at(base, 100), &idle("$ "), SETTLE));
        assert!(!still.settled(at(base, 299), &idle("$ "), SETTLE));
        assert!(still.settled(at(base, 300), &idle("$ "), SETTLE));
        assert!(still.settled(at(base, 350), &idle("$ "), SETTLE));
    }

    #[test]
    fn a_screen_that_moves_starts_the_settle_over() {
        let base = Instant::now();
        let mut still = Still::default();
        assert!(!still.settled(base, &idle("a"), SETTLE));
        assert!(!still.settled(at(base, 250), &idle("a"), SETTLE));
        // The screen changes just before the settle would have been up.
        assert!(!still.settled(at(base, 290), &idle("ab"), SETTLE));
        assert!(
            !still.settled(at(base, 400), &idle("ab"), SETTLE),
            "110 ms of the new screen"
        );
        assert!(
            still.settled(at(base, 590), &idle("ab"), SETTLE),
            "300 ms of the new screen"
        );
    }

    #[test]
    fn a_look_at_a_shell_that_is_not_idle_starts_the_settle_over() {
        let base = Instant::now();
        let mut still = Still::default();
        assert!(!still.settled(base, &idle("$ "), SETTLE));
        assert!(!still.settled(at(base, 200), &busy(), SETTLE));
        // The same screen again, but the time it was idle before the gap does not count.
        assert!(!still.settled(at(base, 400), &idle("$ "), SETTLE));
        assert!(!still.settled(at(base, 699), &idle("$ "), SETTLE));
        assert!(still.settled(at(base, 700), &idle("$ "), SETTLE));
    }

    #[test]
    fn a_shell_that_cannot_be_told_to_be_idle_is_never_believed() {
        let base = Instant::now();
        let mut still = Still::default();
        let unknown = Seen {
            idle: None,
            drawn: true,
            quiet: true,
            screen: Some("$ ".to_string()),
            no_input: None,
            command: String::new(),
        };
        for ms in [0, 400, 4000] {
            assert!(!still.settled(at(base, ms), &unknown, SETTLE), "{ms} ms");
        }
    }

    #[test]
    fn an_idle_shell_whose_screen_could_not_be_read_is_not_believed() {
        let base = Instant::now();
        let mut still = Still::default();
        let blind = Seen {
            idle: Some(true),
            drawn: true,
            quiet: false,
            screen: None,
            no_input: None,
            command: String::new(),
        };
        assert!(!still.settled(base, &blind, SETTLE));
        assert!(!still.settled(at(base, 1000), &blind, SETTLE));
    }

    #[test]
    fn tmux_vouching_for_a_quiet_window_makes_a_shell_ready_on_the_first_look() {
        let seen = Seen {
            idle: Some(true),
            drawn: true,
            quiet: true,
            screen: None,
            no_input: None,
            command: String::new(),
        };
        assert!(Still::default().settled(Instant::now(), &seen, SETTLE));
        // But only for a shell that is idle.
        let not_idle = Seen {
            idle: Some(false),
            ..seen
        };
        assert!(!Still::default().settled(Instant::now(), &not_idle, SETTLE));
    }

    #[test]
    fn a_shell_that_has_drawn_nothing_is_not_believed_however_still_it_has_held() {
        // A shell that has only just started is idle too, and a window that has not drawn in a
        // long while is quiet: what neither says is that a prompt has been drawn.
        let base = Instant::now();
        let mut still = Still::default();
        let blank = Seen {
            idle: Some(true),
            drawn: false,
            quiet: true,
            screen: Some("0,0\n".to_string()),
            no_input: None,
            command: String::new(),
        };
        for ms in [0, 300, 5000] {
            assert!(!still.settled(at(base, ms), &blank, SETTLE), "{ms} ms");
        }
        // Once something is drawn, the settle counts from there and not from before.
        assert!(!still.settled(at(base, 6000), &idle("$ "), SETTLE));
        assert!(!still.settled(at(base, 6299), &idle("$ "), SETTLE));
        assert!(still.settled(at(base, 6300), &idle("$ "), SETTLE));
    }

    #[test]
    fn a_window_is_quiet_once_the_second_it_last_drew_in_is_over_and_the_settle_has_passed() {
        // Output in the second 100 s may have come as late as 101 s; the settle is 300 ms after that.
        let quiet = |now_ms| quiet_for(now_ms, Some(100), SETTLE);
        assert!(!quiet(100_000), "in the same second");
        assert!(!quiet(101_000), "the second over, the settle not begun");
        assert!(!quiet(101_299));
        assert!(quiet(101_300));
        assert!(quiet(160_000));
    }

    #[test]
    fn a_clock_tmux_did_not_print_vouches_for_nothing() {
        assert!(!quiet_for(i64::MAX, None, SETTLE));
        assert!(!quiet_for(0, Some(i64::MAX), SETTLE), "no overflow either");
    }

    #[test]
    fn a_pane_in_a_mode_or_with_its_input_off_takes_no_input_and_is_refused_in_words() {
        assert_eq!(NoInput::of("0", "", "0"), None);
        assert_eq!(
            NoInput::of("1", "copy-mode", "0"),
            Some(NoInput::Mode("copy mode".to_string()))
        );
        assert_eq!(
            NoInput::of("1", "", "1"),
            Some(NoInput::Mode("a mode".to_string()))
        );
        assert_eq!(NoInput::of("0", "", "1"), Some(NoInput::Off));
        assert_eq!(
            NoInput::Mode("copy mode".to_string()).refusal("w1:p3"),
            "pane w1:p3 is in copy mode; leave it and try again"
        );
        assert_eq!(
            NoInput::Off.refusal("w1:p3"),
            "pane w1:p3 takes no input; turn its input back on and try again"
        );
        // Once something was typed, it may be on the line still.
        assert_eq!(
            NoInput::Mode("copy mode".to_string()).refusal_after_typing("w1:p3", "the command"),
            "pane w1:p3 is in copy mode, and the command typed into it may be left on its line; leave \
             the mode, clear the line and try again"
        );
        assert_eq!(
            NoInput::Off.refusal_after_typing("w1:p3", "the prompt"),
            "pane w1:p3 takes no input, and the prompt typed into it may be left on its line; turn its \
             input back on, clear the line and try again"
        );
    }

    #[test]
    fn tmux_having_no_such_pane_is_a_pane_gone_and_nothing_else_is() {
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(1 << 8)
        };
        let failed = |stderr: &str| {
            anyhow::Error::new(CommandFailed {
                command: "tmux display-message".to_string(),
                status,
                stderr: stderr.to_string(),
            })
        };
        assert!(is_gone(&failed("can't find pane: %7")));
        assert!(is_gone(&failed("can't find window: @4")));
        assert!(is_gone(&failed("can't find session: alpha")));
        assert!(is_gone(&failed("no such pane: %7")));
        assert!(is_gone(&failed("no such window: @4")));
        assert!(is_gone(&failed("no such session: alpha")));
        assert!(is_gone(
            &failed("can't find pane: %7").context("reading it")
        ));
        assert!(!is_gone(&failed("no server running on /tmp/tmux-501/x")));
        assert!(!is_gone(&anyhow::anyhow!("can't find pane: %7")));
    }
}
