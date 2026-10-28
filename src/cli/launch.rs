//! Whether the command `taw agent start` typed has started the agent.
//!
//! The verb does not take the kind it was given as the agent's own hint: its wait for the agent
//! would pass on the first look whatever had happened, and with no `claude` installed the pane
//! would show "command not found" while the verb reported success. A typed command has started the
//! agent when the pane's own shell is no longer in front of the terminal, and one of three things
//! says the agent is there:
//!
//! * the process in front of the pane is named for it ([`detect::identify`] with no hint: a
//!   program called `claude`);
//! * its own integration reported it, with `@agent_reported_kind` or with a state a hook wrote,
//!   which is how an agent that no process name gives away says so, and which counts even at a look
//!   where the process table cannot say whether the shell is in front;
//! * the watcher has read a state for it off the screen with its kind's rules. That is any state
//!   but `unknown`, which is what a new agent reads for its first 3 seconds and what a hint with
//!   nothing behind it reads for ever. It is the one word an agent run under `node`, or by a
//!   wrapper script, gives when it reports nothing itself, and it needs the pane's watcher. The
//!   pane still shows what the last agent left when the next is started, and its rules read that
//!   frame as well as they read a new one, so the verb writes where it typed the command
//!   (`@agent_typed_line`, [`TypedLine`]), once the shell has echoed it ([`await_echo`]), and the
//!   watcher reads only the rows below that line while it is on the screen. The state counts once
//!   the screen shows something below the line
//!   ([`TypedLine::drawn_below`]): the rows a shell rewrites on Enter (a transient prompt) are on
//!   the line or above it, and the command's own output, an install's progress, is below it, where
//!   the watcher reads it for what it is.
//!
//! What the pane recorded of an agent before the command was typed is taken off it before it is
//! (`agent start` clears it), so what the pane records while the verb waits was said by what the
//! command started.
//!
//! The shell back at its prompt before any of them is the agent not starting: the command was not
//! found, or the agent exited at once. The pane's own shell is in front when
//! [`crate::watch::pane_shell_is_idle`] says so, the test the wait before typing makes: the
//! terminal's foreground group is led by the pane's process, alone, and it is a shell. The name of
//! the pane's command is no use for that, because tmux reports a script's interpreter, `sh` or
//! `bash`, for as long as the script runs.
//!
//! Three things have to hold before the shell is believed to be back, and each keeps a start that
//! is only slow from being given up on:
//!
//! * the screen is not what it was before the command was typed, because a shell that has not yet
//!   read the line it was typed has changed nothing, and looks as idle as one that has finished;
//! * the cursor is off the left edge, where a prompt leaves it and where a shell that has taken the
//!   line and is working through it (a shell function that loads a version manager before it runs
//!   the agent) has not drawn one yet;
//! * the screen has stayed as it is for [`BACK_FOR`], by two looks that far apart, since the
//!   process table passes for a moment between two commands of a function.

use std::time::{Duration, Instant};

use super::common;
use super::envelope::CliError;
use crate::detect;
use crate::session::prompt;
use crate::store::{AgentRecord, AgentState};
use crate::tmux::Tmux;
use crate::watch::typed_line::{self, TypedLine};

/// How long the pane has to stay as it is, with its shell in front of it, before the shell is
/// believed to be at its prompt: what a shell that has just started is given
/// ([`prompt::SETTLE`]).
const BACK_FOR: Duration = prompt::SETTLE;

/// What one look at the pane found.
#[derive(Debug, Default)]
pub(crate) struct Look {
    /// The pane's own process, its shell (`#{pane_pid}`).
    pub pane_pid: u32,
    /// The pane's foreground command (`#{pane_current_command}`).
    pub command: String,
    /// What the pane records of an agent.
    pub agent: AgentRecord,
    /// Whether the pane's own shell is in front of the terminal, at its prompt with nothing
    /// running in it (`watch::pane_shell_is_idle`); `None` when `ps` or `pgrep` could not say.
    pub shell_idle: Option<bool>,
    /// What the pane shows, taken when the judgement wants it: with its shell in front, and with
    /// something else in front when the record holds a state the watcher read off the screen,
    /// which counts only for a screen that shows something new.
    pub shown: Option<Shown>,
    /// The agent the container runtime in front of the pane runs, read off its arguments
    /// (`watch::container_agent`); `None` for anything else in front.
    pub container: Option<String>,
}

/// What a pane whose shell is in front of it shows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Shown {
    /// The column of the cursor: `0` is the left edge.
    pub cursor_x: u32,
    pub screen: String,
    /// The size of the pane's history (`#{history_size}`), which says how far the screen has scrolled.
    pub history: u64,
    /// Whether the pane is on its alternate screen (`#{alternate_on}`).
    pub alternate: bool,
}

/// What a look comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The agent is there.
    Started,
    /// The shell is back at its prompt and no agent said it was there first.
    DidNotStart,
    /// Neither yet.
    Waiting,
}

/// The wait for one agent, from the moment its command was typed.
pub(crate) struct Launch {
    /// The agent's id, as `@agent_kind` records it.
    kind: String,
    /// The pane's screen just before the command was typed.
    before: String,
    /// Where the command was typed: what it started can have drawn only below it.
    line: TypedLine,
    /// What the pane showed with its shell in front of it, and since when it has shown exactly
    /// that.
    quiet: Option<(Instant, Shown)>,
    /// Whether the command was a `taw sandbox` line, whose agent is there only in a container
    /// ([`Launch::sandboxed`]).
    sandboxed: bool,
}

impl Launch {
    /// The wait for the agent `kind`, whose command was typed on `line` of a pane that showed `before`.
    pub(crate) fn new(kind: &str, before: String, line: TypedLine) -> Launch {
        Launch {
            kind: kind.to_string(),
            before,
            line,
            quiet: None,
            sandboxed: false,
        }
    }

    /// The wait for the agent `kind` that a `taw sandbox` line typed on `line` starts in a container:
    /// it is there when the container runtime in front of the pane runs it, and by nothing else. A
    /// process named for it is the agent run on this machine, outside any container, and a state read
    /// off the screen or a report can come from a profile that keeps a shell in front and has no agent
    /// row. Its shell back at its prompt before that is a start that did not happen, as for any start.
    pub(crate) fn sandboxed(kind: &str, before: String, line: TypedLine) -> Launch {
        Launch {
            sandboxed: true,
            ..Launch::new(kind, before, line)
        }
    }

    /// Takes one look in, at `now`.
    pub(crate) fn judge(&mut self, now: Instant, look: &Look) -> Verdict {
        if self.sandboxed {
            if look.container.as_deref() == Some(self.kind.as_str()) {
                return Verdict::Started;
            }
            return match look.shell_idle {
                Some(true) => self.shell_is_in_front(now, look.shown.as_ref()),
                _ => {
                    self.quiet = None;
                    Verdict::Waiting
                }
            };
        }
        if detect::identify(&look.command, None).as_deref() == Some(self.kind.as_str()) {
            return Verdict::Started;
        }
        match look.shell_idle {
            Some(true) => self.shell_is_in_front(now, look.shown.as_ref()),
            Some(false) => {
                self.quiet = None;
                if self.says_so(look) {
                    Verdict::Started
                } else {
                    Verdict::Waiting
                }
            }
            // The question cannot be put, which a pane it was put to a moment ago did not make
            // a habit of: nothing can be said of the shell either way. The agent's own report, made
            // since the command was typed, needs no shell to be believed; a state read off the screen
            // waits for a look that can tell.
            None => {
                self.quiet = None;
                if evidence(&look.agent, &self.kind) == Evidence::Reported {
                    Verdict::Started
                } else {
                    Verdict::Waiting
                }
            }
        }
    }

    /// What the look `look` at `now` found, and the `verdict` it came to, in one line for the verb's
    /// debug log: what is in front of the pane, whether the pane's own shell is idle, the cursor's
    /// column, whether the screen is still the one from before the command was typed, how long it has
    /// held still with the shell in front, and what the pane records. A wait that runs out its budget
    /// is told by these lines which branch of the judgement it stayed in.
    pub(crate) fn account(&self, now: Instant, look: &Look, verdict: Verdict) -> String {
        let shown = look.shown.as_ref();
        let not_read = || "not read".to_string();
        format!(
            "in front {:?}, shell idle {:?}, cursor x {}, screen as before {}, held still {}, \
             record {:?} {:?}: {verdict:?}",
            look.command,
            look.shell_idle,
            shown.map_or_else(not_read, |shown| shown.cursor_x.to_string()),
            shown.map_or_else(not_read, |shown| (shown.screen == self.before).to_string()),
            self.quiet.as_ref().map_or_else(
                || "no".to_string(),
                |(since, _)| format!(
                    "for {} ms",
                    now.saturating_duration_since(*since).as_millis()
                )
            ),
            look.agent.kind,
            look.agent.state,
        )
    }

    /// A look at a pane whose shell is in front of it: the shell has not taken the line yet, or is
    /// working through it, or is back at its prompt after it. Only the last gives the agent up.
    fn shell_is_in_front(&mut self, now: Instant, shown: Option<&Shown>) -> Verdict {
        let Some(shown) = shown.filter(|shown| shown.cursor_x > 0 && shown.screen != self.before)
        else {
            self.quiet = None;
            return Verdict::Waiting;
        };
        match &self.quiet {
            Some((since, seen)) if seen == shown => {
                if now.saturating_duration_since(*since) >= BACK_FOR {
                    Verdict::DidNotStart
                } else {
                    Verdict::Waiting
                }
            }
            _ => {
                self.quiet = Some((now, shown.clone()));
                Verdict::Waiting
            }
        }
    }

    /// Whether the pane's record says the agent is there, with something other than the pane's
    /// shell in front of it: it does if its own integration reported it, or if the watcher read a
    /// state for it off a screen that shows something below the line the command was typed on.
    fn says_so(&self, look: &Look) -> bool {
        match evidence(&look.agent, &self.kind) {
            Evidence::Reported => true,
            Evidence::Read => look.shown.as_ref().is_some_and(|shown| {
                self.line
                    .drawn_below(&shown.screen, shown.history, shown.alternate)
            }),
            Evidence::Nothing => false,
        }
    }
}

/// What the pane's record says of the agent `kind`.
#[derive(Debug, PartialEq, Eq)]
enum Evidence {
    /// Nothing of this agent: no record, another agent's, or a state that is no state yet.
    Nothing,
    /// A state the watcher read off the screen, which is any but `unknown`.
    Read,
    /// The agent's own integration said it is there: the kind reported (`@agent_reported_kind`), or
    /// a state a hook wrote (`@agent_source` of `hook:<id>`).
    Reported,
}

/// What the pane's record says of the agent `kind`, and no other agent: what the pane records of
/// another is not this one's.
fn evidence(record: &AgentRecord, kind: &str) -> Evidence {
    if record.kind.as_deref() != Some(kind) {
        return Evidence::Nothing;
    }
    let reported = record.reported_kind.as_deref() == Some(kind)
        || record
            .source
            .as_deref()
            .is_some_and(|source| source.starts_with("hook:"));
    if reported {
        return Evidence::Reported;
    }
    match record.state {
        Some(state) if state != AgentState::Unknown => Evidence::Read,
        _ => Evidence::Nothing,
    }
}

/// Looks at `pane_id` once: what it records, what is in front of it and, when that is its shell,
/// what it shows.
pub(crate) fn look_at(t: &Tmux, pane_id: &str) -> anyhow::Result<Look> {
    let info = common::pane_info(t, pane_id)?;
    let shell_idle = crate::watch::pane_shell_is_idle(info.pane_pid);
    let shown = if shell_idle == Some(true) || read_off_the_screen(&info.agent) {
        Some(shown(t, pane_id)?)
    } else {
        None
    };
    Ok(Look {
        pane_pid: info.pane_pid,
        container: crate::watch::container_agent(info.pane_pid, &info.pane_current_command),
        command: info.pane_current_command,
        agent: info.agent,
        shell_idle,
        shown,
    })
}

/// Whether the pane's record holds a state that was read off its screen, and not one its agent
/// reported, which is the only record whose worth depends on the screen it was read from.
fn read_off_the_screen(record: &AgentRecord) -> bool {
    evidence(record, record.kind.as_deref().unwrap_or_default()) == Evidence::Read
}

/// What `pane_id` shows now: the cursor's column, the size of the history and the alternate screen,
/// read in the same tmux invocation as the screen, so that they describe it.
fn shown(t: &Tmux, pane_id: &str) -> anyhow::Result<Shown> {
    let (head, screen) = with_screen(t, pane_id, "#{cursor_x}\t#{history_size}\t#{alternate_on}")?;
    let mut fields = head.split('\t');
    let mut next = || fields.next().unwrap_or_default().trim().to_string();
    Ok(Shown {
        cursor_x: next().parse().unwrap_or(0),
        history: next().parse().unwrap_or(0),
        alternate: next() == "1",
        screen,
    })
}

/// The pane's prompt just before the command is typed: the screen, and where on it the command goes.
pub(crate) struct Prompt {
    pub screen: String,
    cursor_x: u32,
    cursor_y: u32,
    width: u32,
    history: u64,
}

impl Prompt {
    /// The line `typed` ends on, typed at this prompt, told by its row as well ([`Prompt::tell`]).
    pub(crate) fn line_for(&self, typed: &str) -> TypedLine {
        self.tell(
            TypedLine::at(
                self.cursor_x,
                self.cursor_y,
                self.width,
                self.history,
                typed,
            ),
            typed,
        )
    }

    /// `line`, the line `typed` was typed on at this prompt, told by what its row starts with: the prompt
    /// as drawn left of the cursor, and the command ([`typed_line::start_of`]); and by whether the row
    /// showed a right prompt beyond the cursor ([`typed_line::right_of`]).
    pub(crate) fn tell(&self, line: TypedLine, typed: &str) -> TypedLine {
        line.starting_with(Some(typed_line::start_of(
            &self.screen,
            self.cursor_x,
            self.cursor_y,
            self.width,
            typed,
        )))
        .with_a_right_prompt(typed_line::right_of(
            &self.screen,
            self.cursor_x,
            self.cursor_y,
        ))
    }
}

/// How long `agent start` looks for the shell to have echoed the command it typed, before it records
/// the line it typed it on and sends Enter. A shell echoes at once what it is typed; one that takes
/// longer (a machine under load) has the line recorded where the command was typed, which the watcher
/// finds once it is echoed there. Well inside the grace a new agent is given (3 s), in which the watcher
/// reads nothing off the screen.
pub(crate) const ECHO_BOUND: Duration = Duration::from_secs(1);

/// How often the shell is looked at while it echoes.
const ECHO_POLL: Duration = Duration::from_millis(25);

/// What the wait for the shell to echo the command came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Echo {
    /// The shell has echoed it: the line it is on ([`TypedLine::echoed`]).
    Seen(TypedLine),
    /// It had not when the wait ran out.
    NotSeen,
    /// The pane takes no input since the wait for its prompt (a person put it in copy mode): what was
    /// typed went to the mode, or nowhere.
    NoInput(prompt::NoInput),
}

/// Looks at `pane_id`, whose shell was typed `typed` at `prompt`, every [`ECHO_POLL`] for up to
/// `bound`, until the shell has echoed all of it, or the pane is found to take no input. Each look reads
/// the cursor, the size of the history and the screen in one tmux invocation, so that they describe one
/// moment.
pub(crate) fn await_echo(
    t: &Tmux,
    pane_id: &str,
    prompt: &Prompt,
    typed: &str,
    bound: Duration,
) -> anyhow::Result<Echo> {
    let deadline = Instant::now() + bound;
    loop {
        let (head, screen) = with_screen(
            t,
            pane_id,
            "#{cursor_x}\t#{cursor_y}\t#{history_size}\t#{pane_in_mode}\t#{pane_mode}\t#{pane_input_off}",
        )?;
        let fields: Vec<&str> = head.split('\t').collect();
        let field = |index: usize| fields.get(index).copied().unwrap_or_default();
        if let Some(why) = prompt::NoInput::of(field(3), field(4), field(5)) {
            return Ok(Echo::NoInput(why));
        }
        let number = |index: usize| field(index).trim().parse().unwrap_or(0u64);
        let cursor = (
            u32::try_from(number(0)).unwrap_or(u32::MAX),
            u32::try_from(number(1)).unwrap_or(u32::MAX),
        );
        let history = number(2);
        if let Some(line) = TypedLine::echoed(
            prompt.cursor_x,
            prompt.width,
            typed,
            cursor,
            history,
            &screen,
        ) {
            return Ok(Echo::Seen(prompt.tell(line, typed)));
        }
        if Instant::now() >= deadline {
            return Ok(Echo::NotSeen);
        }
        std::thread::sleep(ECHO_POLL);
    }
}

/// The prompt of `pane_id`, read in one tmux invocation: the cursor, the pane's width, the size of its
/// history, and the screen.
pub(crate) fn prompt_of(t: &Tmux, pane_id: &str) -> anyhow::Result<Prompt> {
    let (head, screen) = with_screen(
        t,
        pane_id,
        "#{cursor_x}\t#{cursor_y}\t#{pane_width}\t#{history_size}",
    )?;
    let mut fields = head
        .split('\t')
        .map(|field| field.trim().parse().unwrap_or(0u64));
    let mut next = || fields.next().unwrap_or(0);
    let number = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
    Ok(Prompt {
        cursor_x: number(next()),
        cursor_y: number(next()),
        width: number(next()),
        history: next(),
        screen,
    })
}

/// `format` expanded for `pane_id` and its screen, in one tmux invocation
/// ([`common::display_and_capture`]).
fn with_screen(t: &Tmux, pane_id: &str, format: &str) -> anyhow::Result<(String, String)> {
    common::display_and_capture(t, pane_id, format).map_err(|err| closed_or(err, pane_id))
}

/// `err`, or `not_found` when it is tmux saying the pane was closed since the wait last looked.
fn closed_or(err: anyhow::Error, pane_id: &str) -> anyhow::Error {
    if prompt::is_gone(&err) {
        CliError::not_found(format!(
            "pane '{pane_id}' was closed while the agent was starting"
        ))
    } else {
        err
    }
}

/// What `agent start` says when the shell is back at its prompt and the agent is not there:
/// which agent, which pane (as the caller named it), and where to look for the reason.
pub(crate) fn not_started_message(kind: &str, asked_for: &str) -> String {
    format!(
        "{kind} did not start in pane {asked_for}: its shell is back at its prompt and nothing is \
         running in it. The command may not be installed, or it may have exited at once. \
         `taw pane read {asked_for}` shows what the shell said."
    )
}

/// What `agent start --sandbox` says when its wait runs out with no agent in a container in front.
pub(crate) fn no_agent_seen_message(asked_for: &str) -> String {
    format!(
        "taw saw no agent in pane {asked_for}: a profile must end by running the runtime, with the \
         agent as the container's program"
    )
}

/// What `agent start --sandbox` says when the pane's shell is back before the agent was there.
pub(crate) fn sandbox_not_started_message(kind: &str, asked_for: &str) -> String {
    format!(
        "{kind} did not start in its sandbox in pane {asked_for}: its shell is back at its prompt. \
         `taw pane read {asked_for}` shows what the profile said."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND: &str = "claude";

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// What the pane showed before the command was typed: a prompt.
    const BEFORE: &str = "sh-3.2$ ";

    fn launch() -> Launch {
        Launch::new(KIND, BEFORE.to_string(), TypedLine::at(8, 0, 80, 0, KIND))
    }

    /// The pane's own shell in front, showing `screen` with the cursor at `x`.
    fn shell(x: u32, screen: &str) -> Look {
        Look {
            command: "sh".to_string(),
            shell_idle: Some(true),
            shown: Some(Shown {
                cursor_x: x,
                screen: screen.to_string(),
                ..Shown::default()
            }),
            ..Look::default()
        }
    }

    /// The shell back at its prompt after a command it did not find.
    fn not_found() -> Look {
        shell(8, "sh-3.2$ claude\nsh: claude: command not found\nsh-3.2$ ")
    }

    /// What a pane shows once the command has put something on it: more than the prompt line it
    /// was typed on.
    const DRAWN: &str = "sh-3.2$ claude\n? for shortcuts\n";

    /// Something other than the shell in front of the pane, which a record may say is the agent,
    /// with something drawn on the screen.
    fn running(command: &str, agent: AgentRecord) -> Look {
        Look {
            command: command.to_string(),
            agent,
            shell_idle: Some(false),
            shown: Some(Shown {
                cursor_x: 0,
                screen: DRAWN.to_string(),
                ..Shown::default()
            }),
            ..Look::default()
        }
    }

    fn record(kind: &str) -> AgentRecord {
        AgentRecord {
            kind: Some(kind.to_string()),
            ..AgentRecord::default()
        }
    }

    #[test]
    fn a_process_named_for_the_agent_is_the_agent_whatever_else_is_known() {
        let base = Instant::now();
        for (command, idle) in [
            ("claude", Some(false)),
            ("/opt/homebrew/bin/claude", Some(false)),
            ("claude-code", Some(false)),
            ("CLAUDE", Some(false)),
            ("claude", None),
        ] {
            let look = Look {
                command: command.to_string(),
                shell_idle: idle,
                ..Look::default()
            };
            assert_eq!(
                launch().judge(base, &look),
                Verdict::Started,
                "{command:?} with the shell {idle:?}"
            );
        }
    }

    #[test]
    fn the_names_an_agent_goes_by_count_as_its_own() {
        // `--kind kiro-cli` is the agent `kiro`, and `kiro-cli` is its process.
        let look = Look {
            command: "kiro-cli".to_string(),
            shell_idle: Some(false),
            ..Look::default()
        };
        assert_eq!(
            Launch::new(
                "kiro",
                BEFORE.to_string(),
                TypedLine::at(8, 0, 80, 0, "kiro-cli")
            )
            .judge(Instant::now(), &look),
            Verdict::Started
        );
    }

    #[test]
    fn a_process_named_for_another_agent_is_not_this_one() {
        let look = running("codex", AgentRecord::default());
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Waiting);
    }

    #[test]
    fn an_interpreter_is_no_evidence_by_itself() {
        let base = Instant::now();
        for command in ["sh", "bash", "zsh", "node", "python3", "bun", "sleep"] {
            assert_eq!(
                launch().judge(base, &running(command, AgentRecord::default())),
                Verdict::Waiting,
                "{command}"
            );
        }
    }

    #[test]
    fn a_report_made_after_the_command_was_typed_says_the_agent_is_there_with_another_runtime_in_front()
     {
        // `agent start` has taken what the pane recorded off it before it typed, so a report on
        // the pane now was made by what the command started: it needs no screen to be believed.
        let mut agent = record(KIND);
        agent.reported_kind = Some(KIND.to_string());
        let mut look = running("node", agent);
        look.shown = None;
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Started);
    }

    #[test]
    fn a_state_a_hook_reported_says_the_agent_is_there_even_when_it_is_unknown() {
        let mut agent = record(KIND);
        agent.source = Some("hook:claude".to_string());
        agent.state = Some(AgentState::Unknown);
        assert_eq!(
            launch().judge(Instant::now(), &running("node", agent)),
            Verdict::Started
        );
    }

    #[test]
    fn a_state_the_watcher_read_off_the_screen_says_the_agent_is_there() {
        for state in [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Done,
        ] {
            let mut agent = record(KIND);
            agent.source = Some("manifest".to_string());
            agent.state = Some(state);
            assert_eq!(
                launch().judge(Instant::now(), &running("node", agent)),
                Verdict::Started,
                "{state}"
            );
        }
    }

    #[test]
    fn the_unknown_a_new_agent_reads_for_its_first_seconds_is_not_a_state_read() {
        let mut agent = record(KIND);
        agent.source = Some("manifest".to_string());
        agent.state = Some(AgentState::Unknown);
        assert_eq!(
            launch().judge(Instant::now(), &running("node", agent)),
            Verdict::Waiting
        );
        // Nor is a record with no state yet, or none at all.
        assert_eq!(
            launch().judge(Instant::now(), &running("node", record(KIND))),
            Verdict::Waiting
        );
    }

    #[test]
    fn what_the_pane_records_of_another_agent_is_not_this_ones() {
        let mut agent = record("codex");
        agent.reported_kind = Some("codex".to_string());
        agent.source = Some("hook:codex".to_string());
        agent.state = Some(AgentState::Idle);
        assert_eq!(
            launch().judge(Instant::now(), &running("node", agent.clone())),
            Verdict::Waiting
        );
        // And a state with no agent recorded is not an agent's.
        agent.kind = None;
        assert_eq!(
            launch().judge(Instant::now(), &running("node", agent)),
            Verdict::Waiting
        );
    }

    /// What the pane showed before the command was typed: the last agent's screen, and a prompt.
    const LEFT_BEHIND: &str = "esc to interrupt\n? for shortcuts\nsh-3.2$ ";

    /// The same pane with the command on its prompt line, and nothing drawn.
    const LEFT_BEHIND_TYPED: &str = "esc to interrupt\n? for shortcuts\nsh-3.2$ claude\n";

    fn watcher_read(state: AgentState) -> AgentRecord {
        let mut agent = record(KIND);
        agent.source = Some("manifest".to_string());
        agent.state = Some(state);
        agent
    }

    fn showing(screen: &str) -> Option<Shown> {
        Some(Shown {
            cursor_x: 0,
            screen: screen.to_string(),
            ..Shown::default()
        })
    }

    /// The wait for a `claude` typed on the prompt of [`LEFT_BEHIND`], its third row.
    fn launch_over_the_last_agents_frame() -> Launch {
        Launch::new(
            KIND,
            LEFT_BEHIND.to_string(),
            TypedLine::at(8, 2, 80, 0, KIND),
        )
    }

    #[test]
    fn a_state_read_off_a_screen_nothing_was_drawn_on_is_the_last_agents_and_not_this_ones() {
        // The new process has drawn nothing, so the screen the watcher read the state from is the
        // one the last agent left, and says nothing of this one.
        let mut launch = launch_over_the_last_agents_frame();
        let mut look = running("node", watcher_read(AgentState::Idle));
        look.shown = showing(LEFT_BEHIND_TYPED);
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Waiting);
        // Drawn on, the same record is the agent's.
        look.shown = showing("esc to interrupt\n? for shortcuts\nsh-3.2$ claude\nloading\n");
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Started);
    }

    #[test]
    fn a_state_read_with_no_screen_to_check_it_against_decides_nothing() {
        let mut look = running("node", watcher_read(AgentState::Idle));
        look.shown = None;
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Waiting);
    }

    #[test]
    fn a_state_a_hook_wrote_needs_no_screen_whatever_it_shows() {
        let mut agent = watcher_read(AgentState::Idle);
        agent.source = Some("hook:claude".to_string());
        let mut launch = launch_over_the_last_agents_frame();
        let mut look = running("node", agent);
        look.shown = showing(LEFT_BEHIND_TYPED);
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Started);
    }

    /// A shell that rewrites its prompt rows when the command is entered (a transient prompt, as
    /// powerlevel10k offers) changes the rows above the line and the line itself, and draws nothing
    /// below it: a state the watcher read is no evidence of the agent then. What the command prints
    /// next is below the line.
    #[test]
    fn a_shell_that_rewrites_its_prompt_rows_on_enter_has_drawn_nothing_of_the_agent() {
        let before = "esc to interrupt\n? for shortcuts\n~/work on main\n\u{276f} ";
        let mut launch = Launch::new(KIND, before.to_string(), TypedLine::at(2, 3, 80, 0, KIND));
        let mut look = running("node", watcher_read(AgentState::Blocked));
        look.shown = showing("esc to interrupt\n? for shortcuts\n\u{276f} claude\n\n");
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Waiting);
        look.shown = showing("esc to interrupt\n? for shortcuts\n\u{276f} claude\nWelcome\n");
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Started);
    }

    /// The line is read where the screen has scrolled it to, from the size of the history.
    #[test]
    fn a_state_read_off_a_scrolled_screen_counts_once_something_is_below_the_line() {
        let mut launch = launch_over_the_last_agents_frame();
        let mut look = running("node", watcher_read(AgentState::Idle));
        // Two rows of output scrolled the screen, four rows high, two rows: the line is on its first.
        look.shown = Some(Shown {
            screen: "sh-3.2$ claude\n1\n2\n\n".to_string(),
            history: 2,
            ..Shown::default()
        });
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Started);
        // Nothing scrolled, and nothing is below the line.
        look.shown = Some(Shown {
            screen: "x\ny\nsh-3.2$ claude\n\n".to_string(),
            history: 0,
            ..Shown::default()
        });
        assert_eq!(launch.judge(Instant::now(), &look), Verdict::Waiting);
    }

    #[test]
    fn nothing_says_the_agent_is_there_while_the_shell_is_in_front() {
        // The record is a hook's and the watcher's both, and the shell is back at its prompt
        // after a command that was not found: what the pane recorded of the last occupant says
        // nothing about this one.
        let base = Instant::now();
        let mut stale = record(KIND);
        stale.reported_kind = Some(KIND.to_string());
        stale.source = Some("hook:claude".to_string());
        stale.state = Some(AgentState::Idle);
        let mut look = not_found();
        look.agent = stale;
        let mut launch = launch();
        assert_eq!(launch.judge(base, &look), Verdict::Waiting);
        assert_eq!(launch.judge(at(base, 300), &look), Verdict::DidNotStart);
    }

    #[test]
    fn a_shell_back_at_its_prompt_is_the_agent_not_starting_once_the_screen_has_held_still() {
        let base = Instant::now();
        let mut launch = launch();
        assert_eq!(
            launch.judge(base, &not_found()),
            Verdict::Waiting,
            "the first look only starts the count"
        );
        assert_eq!(launch.judge(at(base, 250), &not_found()), Verdict::Waiting);
        assert_eq!(launch.judge(at(base, 299), &not_found()), Verdict::Waiting);
        assert_eq!(
            launch.judge(at(base, 300), &not_found()),
            Verdict::DidNotStart
        );
        assert_eq!(
            launch.judge(at(base, 900), &not_found()),
            Verdict::DidNotStart
        );
    }

    #[test]
    fn a_screen_that_moves_starts_the_count_over() {
        let base = Instant::now();
        let mut launch = launch();
        assert_eq!(launch.judge(base, &not_found()), Verdict::Waiting);
        let moved = shell(
            8,
            "sh-3.2$ claude\nsh: claude: command not found\nsh-3.2$ x",
        );
        assert_eq!(launch.judge(at(base, 250), &moved), Verdict::Waiting);
        assert_eq!(
            launch.judge(at(base, 500), &moved),
            Verdict::Waiting,
            "250 ms of the new screen"
        );
        assert_eq!(
            launch.judge(at(base, 550), &moved),
            Verdict::DidNotStart,
            "300 ms of the new screen"
        );
    }

    #[test]
    fn a_shell_that_has_not_taken_the_line_yet_has_not_given_the_agent_up() {
        // The pane shows what it showed before the line was typed: the shell is as idle as it was,
        // and has changed nothing, however long that goes on.
        let base = Instant::now();
        let mut launch = launch();
        for ms in [0, 250, 500, 1000, 5000] {
            assert_eq!(
                launch.judge(at(base, ms), &shell(8, BEFORE)),
                Verdict::Waiting,
                "{ms} ms"
            );
        }
    }

    #[test]
    fn a_shell_working_through_the_line_with_nothing_drawn_has_not_given_the_agent_up() {
        // The line is taken, and the cursor is at the left edge of the line after it: a shell
        // function that loads a version manager before it runs the agent looks like this, and the
        // process table says the shell is alone for as long as it works in builtins.
        let base = Instant::now();
        let mut launch = launch();
        let working = shell(0, "sh-3.2$ claude\n");
        for ms in [0, 250, 500, 1000, 5000] {
            assert_eq!(
                launch.judge(at(base, ms), &working),
                Verdict::Waiting,
                "{ms} ms"
            );
        }
    }

    #[test]
    fn the_count_starts_over_when_something_else_takes_the_terminal() {
        let base = Instant::now();
        let mut launch = launch();
        assert_eq!(launch.judge(base, &not_found()), Verdict::Waiting);
        // Something runs, and it is gone again with the screen as it was.
        assert_eq!(
            launch.judge(at(base, 200), &running("sh", AgentRecord::default())),
            Verdict::Waiting
        );
        assert_eq!(
            launch.judge(at(base, 350), &not_found()),
            Verdict::Waiting,
            "the count starts at this look, not at the first"
        );
        assert_eq!(
            launch.judge(at(base, 650), &not_found()),
            Verdict::DidNotStart
        );
    }

    #[test]
    fn a_look_that_cannot_tell_whether_the_shell_is_in_front_decides_nothing() {
        let base = Instant::now();
        let mut launch = launch();
        assert_eq!(launch.judge(base, &not_found()), Verdict::Waiting);
        let mut blind = not_found();
        blind.shell_idle = None;
        blind.shown = None;
        assert_eq!(launch.judge(at(base, 400), &blind), Verdict::Waiting);
        // The count was broken by it, and a record that would have said so is not believed
        // while nothing says the shell has left.
        let mut agent = record(KIND);
        agent.state = Some(AgentState::Idle);
        blind.agent = agent;
        assert_eq!(launch.judge(at(base, 800), &blind), Verdict::Waiting);
        assert_eq!(launch.judge(at(base, 850), &not_found()), Verdict::Waiting);
    }

    #[test]
    fn a_report_says_the_agent_is_there_when_the_shell_cannot_be_told_to_be_in_front_or_not() {
        // `ps` or `pgrep` had no answer for this look. The agent's own integration reported it since the
        // command was typed, which no shell does: that needs no process table to be believed, and the
        // verb, started, goes on to record the agent's pid.
        let mut agent = record(KIND);
        agent.reported_kind = Some(KIND.to_string());
        let look = Look {
            command: "node".to_string(),
            agent,
            shell_idle: None,
            ..Look::default()
        };
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Started);
        // And so does a state its hook wrote.
        let mut agent = record(KIND);
        agent.source = Some("hook:claude".to_string());
        agent.state = Some(AgentState::Idle);
        let look = Look {
            agent,
            shell_idle: None,
            ..Look::default()
        };
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Started);
        // A state the watcher read off the screen waits for a look that can tell, drawn below the line
        // or not.
        let look = Look {
            agent: watcher_read(AgentState::Idle),
            shell_idle: None,
            shown: showing("sh-3.2$ claude\nloading\n"),
            ..Look::default()
        };
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Waiting);
    }

    #[test]
    fn a_shell_with_nothing_shown_is_not_believed_to_be_at_its_prompt() {
        let look = Look {
            command: "sh".to_string(),
            shell_idle: Some(true),
            shown: None,
            ..Look::default()
        };
        assert_eq!(launch().judge(Instant::now(), &look), Verdict::Waiting);
    }

    #[test]
    fn each_look_is_accounted_for_with_what_the_judgement_turned_on() {
        let base = Instant::now();
        let mut launch = launch();
        let look = shell(8, BEFORE);
        let verdict = launch.judge(base, &look);
        assert_eq!(
            launch.account(base, &look, verdict),
            "in front \"sh\", shell idle Some(true), cursor x 8, screen as before true, held still no, \
             record None None: Waiting"
        );
        let look = not_found();
        let verdict = launch.judge(base, &look);
        let later = at(base, 120);
        assert_eq!(
            launch.account(later, &look, verdict),
            "in front \"sh\", shell idle Some(true), cursor x 8, screen as before false, held still \
             for 120 ms, record None None: Waiting"
        );
        let blind = Look {
            command: "node".to_string(),
            shell_idle: None,
            ..Look::default()
        };
        let verdict = launch.judge(later, &blind);
        assert_eq!(
            launch.account(later, &blind, verdict),
            "in front \"node\", shell idle None, cursor x not read, screen as before not read, held \
             still no, record None None: Waiting"
        );
    }

    #[test]
    fn what_agent_start_says_when_the_agent_did_not_start_names_the_agent_and_the_pane() {
        let message = not_started_message("claude", "w0:p1");
        assert!(
            message.starts_with("claude did not start in pane w0:p1: "),
            "{message}"
        );
        assert!(message.contains("taw pane read w0:p1"), "{message}");
    }

    /// Everything it says is in a person's words, with none of the marks kept out of anything
    /// a user reads: an em-dash, an en-dash, an arrow or a curly quote.
    #[test]
    fn what_agent_start_says_when_the_agent_did_not_start_holds_no_dash_arrow_or_curly_quote() {
        let message = not_started_message("claude", "%3");
        for banned in [
            '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
        ] {
            assert!(!message.contains(banned), "{message}");
        }
    }

    /// The wait for a `taw sandbox claude` typed on the prompt of [`BEFORE`].
    fn sandboxed() -> Launch {
        Launch::sandboxed(
            KIND,
            BEFORE.to_string(),
            TypedLine::at(8, 0, 80, 0, "taw sandbox claude"),
        )
    }

    /// Something in front of the pane: `command`, its container running `container`, the record
    /// saying what the watcher read, with something drawn below the line.
    fn in_front(command: &str, container: Option<&str>, record: AgentRecord) -> Look {
        Look {
            pane_pid: 100,
            command: command.to_string(),
            agent: record,
            shell_idle: Some(false),
            shown: Some(Shown {
                cursor_x: 2,
                screen: DRAWN.to_string(),
                ..Shown::default()
            }),
            container: container.map(str::to_string),
        }
    }

    #[test]
    fn a_sandboxed_start_is_there_when_the_runtime_in_front_runs_the_agent() {
        let now = Instant::now();
        assert_eq!(
            sandboxed().judge(
                now,
                &in_front("docker", Some("claude"), AgentRecord::default())
            ),
            Verdict::Started
        );
        assert_eq!(
            sandboxed().judge(
                now,
                &in_front("docker", Some("codex"), AgentRecord::default())
            ),
            Verdict::Waiting
        );
        assert_eq!(
            sandboxed().judge(now, &in_front("docker", None, AgentRecord::default())),
            Verdict::Waiting
        );
    }

    #[test]
    fn a_screen_read_or_a_report_is_no_sandboxed_start() {
        let read = AgentRecord {
            kind: Some(KIND.into()),
            state: Some(AgentState::Idle),
            ..AgentRecord::default()
        };
        let reported = AgentRecord {
            kind: Some(KIND.into()),
            reported_kind: Some(KIND.into()),
            ..AgentRecord::default()
        };
        let now = Instant::now();
        assert_eq!(
            sandboxed().judge(now, &in_front("bash", None, read.clone())),
            Verdict::Waiting
        );
        assert_eq!(
            sandboxed().judge(now, &in_front("bash", None, reported.clone())),
            Verdict::Waiting
        );
        // Nor when the process table could not say whether the shell is in front.
        let mut blind = in_front("bash", None, reported);
        blind.shell_idle = None;
        assert_eq!(sandboxed().judge(now, &blind), Verdict::Waiting);
        // The same look starts an ordinary start, which is what the sandboxed one must not copy.
        assert_eq!(
            launch().judge(now, &in_front("bash", None, read)),
            Verdict::Started
        );
    }

    /// A profile that runs the agent on this machine, outside any container, has started no sandboxed
    /// agent, though a process named for it is in front: what an ordinary start takes for the agent.
    #[test]
    fn the_agent_run_outside_a_container_is_no_sandboxed_start() {
        let now = Instant::now();
        let on_this_machine = in_front("claude", None, AgentRecord::default());
        assert_eq!(sandboxed().judge(now, &on_this_machine), Verdict::Waiting);
        assert_eq!(launch().judge(now, &on_this_machine), Verdict::Started);
    }

    #[test]
    fn a_sandboxed_start_whose_shell_is_back_did_not_start() {
        let mut launching = sandboxed();
        let base = Instant::now();
        let back = shell(
            4,
            "sh-3.2$ taw sandbox claude\ntaw sandbox default: no recipe for claude\nsh-3.2$ ",
        );
        assert_eq!(launching.judge(base, &back), Verdict::Waiting);
        assert_eq!(
            launching.judge(base + BACK_FOR, &back),
            Verdict::DidNotStart
        );
    }

    #[test]
    fn what_a_sandboxed_start_says_when_it_fails() {
        assert_eq!(
            no_agent_seen_message("w1:p3"),
            "taw saw no agent in pane w1:p3: a profile must end by running the runtime, with the agent as the container's program"
        );
        assert_eq!(
            sandbox_not_started_message("claude", "w1:p3"),
            "claude did not start in its sandbox in pane w1:p3: its shell is back at its prompt. `taw pane read w1:p3` shows what the profile said."
        );
        for message in [
            no_agent_seen_message("%3"),
            sandbox_not_started_message("claude", "%3"),
        ] {
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
            ] {
                assert!(!message.contains(banned), "{message}");
            }
        }
    }
}
