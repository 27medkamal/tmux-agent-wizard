use super::*;

const SESSION: &str = "5b2e9c1a-3f47-4d8e-a6b0-7c19e2d4f863";
const OTHER_SESSION: &str = "0d8f4e2b-1c6a-4f39-b7e5-9a2c3d1e8f70";
const SHELL: u32 = 500;

/// The pane's processes as a test sets them: who leads the terminal's foreground group, and the
/// agent's row `ps` prints (`None`: gone). Counts the looks.
struct Fake {
    foreground: u32,
    agent: Option<(u32, &'static str, &'static str)>,
    foreground_looks: usize,
    ps_looks: usize,
}

impl Fake {
    fn new(foreground: u32) -> Fake {
        Fake {
            foreground,
            agent: None,
            foreground_looks: 0,
            ps_looks: 0,
        }
    }

    /// `pid` stopped (`T`) or running (`S+` in front, `S` in the background) as `name`.
    fn agent(&mut self, pid: u32, stat: &'static str, name: &'static str) {
        self.agent = Some((pid, stat, name));
    }
}

impl Look for Fake {
    fn foreground(&mut self) -> Option<u32> {
        self.foreground_looks += 1;
        Some(self.foreground)
    }
    fn shell(&mut self) -> Option<u32> {
        Some(SHELL)
    }
    fn ps(&mut self, pids: &str) -> Option<String> {
        self.ps_looks += 1;
        let tpgid = self.foreground;
        let mut out = String::new();
        if let Some((pid, stat, name)) = self.agent
            && pids.split(',').any(|asked| asked == pid.to_string())
        {
            out.push_str(&format!("{pid} {pid} {tpgid} ttys004 {stat} {name}\n"));
        }
        out.push_str(&format!("{SHELL} {SHELL} {tpgid} ttys004 Ss bash\n"));
        Some(out)
    }
}

fn claude_in_front(session: Option<&'static str>) -> Seen<'static> {
    Seen {
        kind: Some("claude"),
        in_front: true,
        command: "claude",
        session,
    }
}

fn shell_in_front(session: Option<&'static str>) -> Seen<'static> {
    Seen {
        kind: Some("claude"),
        in_front: false,
        command: "bash",
        session,
    }
}

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

#[test]
fn ctrl_z_on_a_noted_agent_reads_as_suspended_and_fg_as_in_front_again() {
    let base = Instant::now();
    let mut note = Note::default();
    let mut look = Fake::new(700);
    look.agent(700, "S+", "claude");
    let read = note.read(&claude_in_front(Some(SESSION)), base, &mut look);
    assert_eq!(
        read,
        Read {
            suspended: false,
            changed: true
        }
    );
    assert_eq!(note.process().map(|process| process.pid), Some(700));

    // Ctrl+Z.
    look.foreground = SHELL;
    look.agent(700, "T", "claude");
    let read = note.read(&shell_in_front(Some(SESSION)), at(base, 100), &mut look);
    assert_eq!(
        read,
        Read {
            suspended: true,
            changed: false
        }
    );

    // `fg`: in front again, the same process, not suspended.
    look.foreground = 700;
    look.agent(700, "S+", "claude");
    let read = note.read(&claude_in_front(Some(SESSION)), at(base, 200), &mut look);
    assert_eq!(read, Read::default());
    assert_eq!(note.process().map(|process| process.pid), Some(700));

    // And it exits: the shell is back, and the process gone.
    look.foreground = SHELL;
    look.agent = None;
    let read = note.read(&shell_in_front(Some(SESSION)), at(base, 300), &mut look);
    assert_eq!(
        read,
        Read {
            suspended: false,
            changed: true
        }
    );
    assert_eq!(note.process(), None);
}

#[test]
fn a_session_reported_while_the_agent_is_in_front_is_its_own_at_the_ctrl_z() {
    let base = Instant::now();
    let mut note = Note::default();
    let mut look = Fake::new(700);
    look.agent(700, "S+", "claude");
    note.read(&claude_in_front(Some(SESSION)), base, &mut look);
    // `/clear` or `/resume` in Claude: its hook reports another session while it stays in front.
    note.read(
        &claude_in_front(Some(OTHER_SESSION)),
        at(base, 100),
        &mut look,
    );
    look.foreground = SHELL;
    look.agent(700, "T", "claude");
    assert!(
        note.read(
            &shell_in_front(Some(OTHER_SESSION)),
            at(base, 200),
            &mut look
        )
        .suspended,
        "the session it reported in front took for another agent's"
    );
}

#[test]
fn another_session_reported_while_the_agent_is_out_of_front_ends_the_note() {
    let base = Instant::now();
    let mut note = Note::default();
    let mut look = Fake::new(700);
    look.agent(700, "S+", "claude");
    note.read(&claude_in_front(Some(SESSION)), base, &mut look);
    look.foreground = SHELL;
    look.agent(700, "T", "claude");
    assert!(
        note.read(&shell_in_front(Some(SESSION)), at(base, 100), &mut look)
            .suspended
    );
    // Another agent, run from the shell, reports its session in the pane.
    let read = note.read(
        &shell_in_front(Some(OTHER_SESSION)),
        at(base, 200),
        &mut look,
    );
    assert_eq!(
        read,
        Read {
            suspended: false,
            changed: true
        }
    );
    assert_eq!(note.process(), None);
}

#[test]
fn a_same_named_program_that_takes_the_agents_place_in_front_is_noted_within_a_second() {
    let base = Instant::now();
    let mut note = Note::default();
    let mut look = Fake::new(700);
    look.agent(700, "S+", "claude");
    note.read(&claude_in_front(None), base, &mut look);
    // The first Claude exits and a second one starts, with no read between that finds the shell.
    look.foreground = 800;
    look.agent(800, "S+", "claude");
    note.read(&claude_in_front(None), at(base, 300), &mut look);
    assert_eq!(
        note.process().map(|process| process.pid),
        Some(700),
        "looked again inside a second"
    );
    let read = note.read(&claude_in_front(None), at(base, 1_100), &mut look);
    assert!(read.changed);
    assert_eq!(note.process().map(|process| process.pid), Some(800));
    look.foreground = SHELL;
    look.agent(800, "T", "claude");
    assert!(
        note.read(&shell_in_front(None), at(base, 1_200), &mut look)
            .suspended
    );
}

#[test]
fn the_shells_own_group_in_front_is_never_noted_as_the_agent() {
    let base = Instant::now();
    let mut note = Note::default();
    // tmux still named Claude, but Ctrl+Z gave the shell the terminal before the look.
    let mut look = Fake::new(SHELL);
    let read = note.read(&claude_in_front(None), base, &mut look);
    assert_eq!(read, Read::default());
    assert_eq!(note.process(), None);
}

#[test]
fn a_suspended_agent_is_looked_at_once_a_second_unless_the_panes_command_changes() {
    let base = Instant::now();
    let mut note = Note::default();
    let mut look = Fake::new(700);
    look.agent(700, "S+", "claude");
    note.read(&claude_in_front(None), base, &mut look);
    let in_front_looks = look.foreground_looks;
    // Reads in front inside the second: no look at all.
    note.read(&claude_in_front(None), at(base, 300), &mut look);
    note.read(&claude_in_front(None), at(base, 600), &mut look);
    assert_eq!(look.foreground_looks, in_front_looks);

    look.foreground = SHELL;
    look.agent(700, "T", "claude");
    assert!(
        note.read(&shell_in_front(None), at(base, 700), &mut look)
            .suspended
    );
    assert_eq!(look.ps_looks, 1, "a look as it leaves the front");
    for ms in [800, 1_000, 1_600] {
        assert!(
            note.read(&shell_in_front(None), at(base, ms), &mut look)
                .suspended
        );
    }
    assert_eq!(look.ps_looks, 1, "no look inside the second");
    let vim = Seen {
        command: "vim",
        ..shell_in_front(None)
    };
    assert!(note.read(&vim, at(base, 1_650), &mut look).suspended);
    assert_eq!(look.ps_looks, 2, "a look when the command changes");
    assert!(note.read(&vim, at(base, 2_650), &mut look).suspended);
    assert_eq!(look.ps_looks, 3, "and once the second has passed");
    // Killed meanwhile: the next look ends it.
    look.agent = None;
    assert!(!note.read(&vim, at(base, 3_700), &mut look).suspended);
}

#[test]
fn a_watcher_started_during_a_suspend_takes_back_the_note_only_for_a_stopped_agent() {
    let base = Instant::now();
    let seen = shell_in_front(Some(SESSION));
    let mut look = Fake::new(SHELL);
    look.agent(700, "T", "claude");
    let mut note = Note::default();
    assert!(!note.start(Some("700 claude"), &seen, base, &mut look));
    assert_eq!(
        note.process(),
        Some(&AgentProcess {
            pid: 700,
            kind: None,
            command: "claude".to_string(),
            session: Some(SESSION.to_string()),
        })
    );
    let read = note.read(&seen, at(base, 50), &mut look);
    assert_eq!(
        read,
        Read {
            suspended: true,
            changed: false
        }
    );

    // Running in the background, gone, another program, or not the agent the pane records: the
    // pane's copy goes.
    for (stat, name, saved) in [
        ("S", "claude", "700 claude"),
        ("T", "vim", "700 claude"),
        ("T", "vim", "700 vim"),
    ] {
        let mut look = Fake::new(SHELL);
        look.agent(700, stat, name);
        let mut note = Note::default();
        assert!(
            note.start(Some(saved), &seen, base, &mut look),
            "{stat} {name} {saved}"
        );
        assert_eq!(note.process(), None);
    }
    let mut gone = Fake::new(SHELL);
    let mut note = Note::default();
    assert!(note.start(Some("700 claude"), &seen, base, &mut gone));
    assert!(
        !note.start(None, &seen, base, &mut gone),
        "nothing kept, nothing to remove"
    );
}

/// An agent found through its job or a container (a `bash` wrapper, a `node` Codex, a `docker`
/// sandbox) is no program `identify` names by its command, so the note carries the kind the pane
/// recorded, and a watcher started while it is stopped compares that with the record's.
#[test]
fn a_watcher_started_during_a_suspend_takes_back_an_agent_found_through_its_job_or_container() {
    let base = Instant::now();
    let seen = shell_in_front(Some(SESSION));
    for (command, saved) in [
        ("bash", "700 claude bash"),
        ("node", "700 claude node"),
        ("docker", "700 claude docker"),
    ] {
        let mut look = Fake::new(SHELL);
        look.agent(700, "T", command);
        let mut note = Note::default();
        assert!(
            !note.start(Some(saved), &seen, base, &mut look),
            "{saved} is the pane's agent behind its shell"
        );
        assert_eq!(note.process().map(|process| process.pid), Some(700));
        assert!(note.read(&seen, at(base, 50), &mut look).suspended);
    }
    // The kind the note carries is the one the record is compared with: another agent's is not it.
    let mut look = Fake::new(SHELL);
    look.agent(700, "T", "node");
    let mut note = Note::default();
    assert!(note.start(Some("700 codex node"), &seen, base, &mut look));
    assert_eq!(note.process(), None);
    // A note the branch wrote before the kind was in it is read as it was: by the command's name.
    let mut look = Fake::new(SHELL);
    look.agent(700, "T", "node");
    let mut note = Note::default();
    assert!(note.start(Some("700 node"), &seen, base, &mut look));
    assert_eq!(note.process(), None);
    let mut look = Fake::new(SHELL);
    look.agent(700, "T", "claude");
    let mut note = Note::default();
    assert!(!note.start(Some("700 claude"), &seen, base, &mut look));
    assert_eq!(note.process().map(|process| process.pid), Some(700));
}

/// What the watcher writes for an agent that is in front, so that the kind reaches the pane.
#[test]
fn the_note_written_for_an_agent_in_front_carries_its_kind() {
    let mut note = Note::default();
    let mut look = Fake::new(700);
    look.agent(700, "S+", "bash");
    let in_front = Seen {
        command: "bash",
        ..claude_in_front(Some(SESSION))
    };
    let read = note.read(&in_front, Instant::now(), &mut look);
    assert!(read.changed);
    assert_eq!(
        note.process().map(AgentProcess::option_value).as_deref(),
        Some("700 claude bash")
    );
}

#[test]
fn a_process_stopped_or_in_the_background_on_the_panes_terminal_is_behind_its_shell() {
    let stopped = "  700   700   500 ttys004  T    claude\n  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(
        behind_the_panes_shell(stopped, 700, 500, "claude"),
        Some(true)
    );
    let background =
        "  700   700   500 ttys004  S    claude\n  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(
        behind_the_panes_shell(background, 700, 500, "claude"),
        Some(false)
    );
    let in_front = "  700   700   700 ttys004  S+   claude\n  500   500   700 ttys004  Ss   bash\n";
    assert_eq!(behind_the_panes_shell(in_front, 700, 500, "claude"), None);
    let gone = "  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(behind_the_panes_shell(gone, 700, 500, "claude"), None);
    let reused = "  700   700   500 ttys004  T    vim\n  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(behind_the_panes_shell(reused, 700, 500, "claude"), None);
    let elsewhere =
        "  700   700   900 ttys009  T    claude\n  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(behind_the_panes_shell(elsewhere, 700, 500, "claude"), None);
}

#[cfg(target_os = "macos")]
#[test]
fn a_command_longer_than_ucomm_is_told_by_its_first_sixteen_bytes_on_macos() {
    let command = "claude-wrapper-for-work";
    let stopped =
        "  700   700   500 ttys004  T    claude-wrapper-f\n  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(
        behind_the_panes_shell(stopped, 700, 500, command),
        Some(true)
    );
    let other =
        "  700   700   500 ttys004  T    claude-wrapper-x\n  500   500   500 ttys004  Ss   bash\n";
    assert_eq!(behind_the_panes_shell(other, 700, 500, command), None);
}

#[test]
fn another_agent_reporting_a_session_in_the_pane_takes_the_suspended_ones_place() {
    assert!(same_agent(Some(SESSION), Some(SESSION)));
    assert!(same_agent(None, Some(SESSION)), "its own first report");
    assert!(same_agent(Some(SESSION), None));
    assert!(!same_agent(Some(SESSION), Some(OTHER_SESSION)));
}

#[test]
fn the_panes_copy_of_the_note_reads_back() {
    let process = AgentProcess {
        pid: 700,
        kind: Some("claude".to_string()),
        command: "claude code".to_string(),
        session: Some(SESSION.to_string()),
    };
    assert_eq!(process.option_value(), "700 claude claude code");
    assert_eq!(
        AgentProcess::parse(&process.option_value(), Some(SESSION)),
        Some(process)
    );
    let older = AgentProcess {
        pid: 700,
        kind: None,
        command: "claude".to_string(),
        session: None,
    };
    assert_eq!(older.option_value(), "700 claude");
    assert_eq!(AgentProcess::parse("700 claude", None), Some(older));
    assert_eq!(AgentProcess::parse("0 claude", None), None);
    assert_eq!(AgentProcess::parse("700", None), None);
    assert_eq!(AgentProcess::parse("x claude", None), None);
}
