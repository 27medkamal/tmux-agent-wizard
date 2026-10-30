//! The popup's snapshot, built from a real tmux server.
//!
//! `tui::data` is unit-tested against hand-built `PaneInfo`s; this checks the other half
//! — that what tmux actually reports (session order, window and pane indices, the
//! `@agent_*` options the watcher writes) lands in the rows the sidebar draws, with the
//! sort and the workspace rollup the sidebar shows.

mod clients;

mod common;

use common::shell_env::ShellHome;
use std::time::Duration;

use clients::Client;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taw::config::{AgentPanelSort, Config, Depth, Layout};
use taw::paths::Paths;
use taw::store::{self, AgentState};
use taw::tmux::Tmux;
use taw::tui::app::App;
use taw::tui::data::{self, GitWorker};
use taw::tui::theme;
use taw::tui::tree::{self, Row};

/// A throwaway tmux server on its own `-L` socket, torn down on drop. Never touches the
/// user's real server: no `-L`/`-S` would mean the default socket, and this always sets
/// one. Same shape as `tests/tmux_client.rs`'s guard, including the per-test counter that
/// keeps concurrently running tests on separate sockets.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-tui-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        // `-f /dev/null` so the host's own tmux.conf (base-index, hooks, TPM autoload)
        // cannot change what this test observes.
        home.start_server(&t, &["-d", "-s", "api", "-x", "80", "-y", "24", "/bin/sh"]);
        // `/bin/sh` here *and* as `default-shell` below: `-f /dev/null` isolates tmux's
        // config but not the *shell's*. The host login shell may print banners, run prompt
        // tools, or open an interactive prompt of its own — in this repo, `mise`'s "trust
        // this config?" question in a fresh worktree — which makes any test that types into
        // a pane flaky for reasons that have nothing to do with `taw`. `/bin/sh` reads no rc
        // file. It has to be named twice because the server does not outlive a
        // `start-server` with no sessions, so there is no earlier moment at which to set the
        // option for this first pane. (Shared with `tests/common/mod.rs`, which does the
        // same for the `e2e_*` files.)
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        Server(t, home)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// `api` (two windows, an agent in each) and `docs` (one window, no agent).
fn two_workspaces() -> (Server, Config) {
    let s = Server::start();
    let t = &s.0;
    t.run_ok(&["new-window", "-t", "api", "-n", "review"])
        .unwrap();
    t.run_ok(&["new-session", "-d", "-s", "docs", "-c", "/tmp"])
        .unwrap();

    let panes = t.list_panes().unwrap();
    let api: Vec<_> = panes.iter().filter(|p| p.session_name == "api").collect();
    assert_eq!(api.len(), 2, "one pane per window");
    let first = api.iter().min_by_key(|p| p.window_index).unwrap();
    let second = api.iter().max_by_key(|p| p.window_index).unwrap();

    // The watcher's own writes: a blocked claude in one window, a working codex in the
    // other. Nothing in this test computes state — it only reads what tmux was told.
    store::write_state(
        t,
        &first.pane_id,
        AgentState::Blocked,
        Some("permission"),
        "manifest",
        100,
    )
    .unwrap();
    t.set_pane_option(&first.pane_id, "@agent_kind", "claude")
        .unwrap();
    t.set_pane_option(&first.pane_id, "@agent_name", "reviewer")
        .unwrap();
    store::write_state(
        t,
        &second.pane_id,
        AgentState::Working,
        None,
        "manifest",
        300,
    )
    .unwrap();
    t.set_pane_option(&second.pane_id, "@agent_kind", "codex")
        .unwrap();

    (s, Config::default())
}

fn snapshot(t: &Tmux, cfg: &Config) -> data::Snapshot {
    snapshot_waiting_for_git(t, cfg, None)
}

/// Builds a snapshot; when `wait_for` names a workspace, rebuilds until the background
/// git worker has answered for it.
///
/// The worker answers asynchronously and the first build only queues the paths, so a
/// test that asserts on git columns has to wait for its own workspace — waiting for
/// "any workspace with git info" would race, because the test server's first session
/// inherits this repository as its cwd and usually answers first.
///
/// No origin pane: these tests are about the listings, not about which row the cursor
/// opens on (`build_takes_the_pane_the_popup_was_opened_from_as_an_argument` is).
fn snapshot_waiting_for_git(t: &Tmux, cfg: &Config, wait_for: Option<&str>) -> data::Snapshot {
    let git = GitWorker::new(Duration::from_secs(2));
    let mut snap = data::build(t, cfg, &git, None, None).unwrap();
    let Some(session) = wait_for else {
        return snap;
    };
    for _ in 0..200 {
        if snap
            .spaces
            .iter()
            .any(|s| s.session_name == session && s.git.is_some())
        {
            return snap;
        }
        std::thread::sleep(Duration::from_millis(10));
        snap = data::build(t, cfg, &git, None, None).unwrap();
    }
    panic!("the git worker never answered for workspace '{session}'");
}

#[test]
fn the_snapshot_lists_the_agent_panes_tmux_reports() {
    let (s, cfg) = two_workspaces();
    let snap = snapshot(&s.0, &cfg);

    assert_eq!(snap.agents.len(), 2, "only the two panes with @agent_* set");
    let kinds: Vec<&str> = snap
        .agents
        .iter()
        .map(|a| a.kind.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(kinds, vec!["claude", "codex"]);
    let blocked = &snap.agents[0];
    assert_eq!(blocked.session_name, "api");
    assert_eq!(blocked.state, Some(AgentState::Blocked));
    assert_eq!(blocked.state_since, 100);
    assert_eq!(blocked.name.as_deref(), Some("reviewer"));
    assert_eq!(blocked.agent_label(), "reviewer");
    assert!(blocked.pane_id.starts_with('%'));
    assert!(blocked.window_id.starts_with('@'));
    // The second agent is in the next window of the same workspace.
    assert_eq!(snap.agents[1].session_name, "api");
    assert!(snap.agents[1].window_index > blocked.window_index);
}

/// A pane tmux titled itself, with the host name, is a popup row with its kind and no title,
/// where a pane a program titled shows that title. Drawn from a real server's snapshot, so it
/// is what the popup shows, and not what a fixture says it would.
#[test]
fn an_agent_pane_nobody_titled_is_a_popup_row_with_its_kind_and_no_title() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (s, cfg) = two_workspaces();
    let t = &s.0;
    let host = taw::tmux::default_pane_title().expect("this machine has a host name");
    // The claude pane is named `reviewer` and gets a title of a program's. The codex pane has
    // no name, so its row says its kind, and no program has titled it.
    let claude = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.agent.kind.as_deref() == Some("claude"))
        .unwrap();
    t.run_ok(&[
        "select-pane",
        "-t",
        &claude.pane_id,
        "-T",
        "fix flaky tests",
    ])
    .unwrap();

    let snap = snapshot(t, &cfg);
    let agents = snap.agents.clone();
    let theme = theme::resolve("catppuccin", &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|f| taw::tui::view::draw(f, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let screen: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    let shown = screen.join("\n");

    assert!(
        shown.contains("codex"),
        "the unnamed agent's row says its kind:\n{shown}"
    );
    assert!(shown.contains("reviewer"), "{shown}");
    assert!(
        shown.contains("fix flaky tests"),
        "a program's title is shown:\n{shown}"
    );
    assert!(
        !shown.contains(&host),
        "the host name is nobody's title, and the popup shows none:\n{shown}"
    );

    // And the rows the popup was drawn from.
    let title_of = |kind: &str| {
        agents
            .iter()
            .find(|a| a.kind.as_deref() == Some(kind))
            .unwrap()
            .pane_title
            .clone()
    };
    assert_eq!(title_of("codex"), "", "tmux's own title is no title");
    assert_eq!(title_of("claude"), "fix flaky tests");
}

/// An agent row is found by its title, whole, though the row draws it cut: the words at the cut
/// end of a long one find the row, and letters scattered through it do not. From a real server's
/// snapshot, through the filter, to the screen.
#[test]
fn the_filter_finds_an_agent_row_by_a_title_the_row_cuts_off() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (s, cfg) = two_workspaces();
    let t = &s.0;
    let claude = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.agent.kind.as_deref() == Some("claude"))
        .unwrap();
    t.run_ok(&[
        "select-pane",
        "-t",
        &claude.pane_id,
        "-T",
        "Reconcile the ledger against the bank statement for october",
    ])
    .unwrap();

    // The popup over the server as it is, `/` and `typed`, drawn as a screen.
    let screen = |typed: &str| -> String {
        let snap = snapshot(t, &cfg);
        let theme = theme::resolve("catppuccin", &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), snap);
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in typed.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|f| taw::tui::view::draw(f, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // The row draws the start of the title and a cut, never the word the query is.
    let unfiltered = screen("");
    assert!(unfiltered.contains("Reconcile the"), "{unfiltered}");
    assert!(!unfiltered.contains("october"), "{unfiltered}");

    let found = screen("october");
    assert!(
        found.contains("reviewer") && found.contains("Reconcile the"),
        "a word at the cut end finds the row:\n{found}"
    );
    assert!(!found.contains("nothing matches"), "{found}");
    assert!(
        !found.contains("codex"),
        "only that row and its header:\n{found}"
    );

    // Positive first (above): the same letters scattered through the title find nothing.
    let scattered = screen("ledgrk");
    assert!(
        scattered.contains("nothing matches \"ledgrk\""),
        "{scattered}"
    );
    assert!(!scattered.contains("reviewer"), "{scattered}");
}

/// At the sessions depth the filter finds a folded header only by the words it draws: its name finds
/// it (api and not docs), and the state word or the kind of an agent it folds finds nothing, as the
/// header draws neither. From a real server's snapshot, through the filter, to the screen.
#[test]
fn at_the_sessions_depth_a_header_is_found_by_its_name_and_not_by_the_agents_it_folds() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (s, mut cfg) = two_workspaces();
    cfg.ui.depth = Depth::Sessions;
    let t = &s.0;

    let screen = |typed: &str| -> String {
        let snap = snapshot(t, &cfg);
        let theme = theme::resolve("catppuccin", &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), snap);
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in typed.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|f| taw::tui::view::draw(f, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Both sessions are there to begin with, folded: no agent row is drawn.
    let unfiltered = screen("");
    assert!(
        unfiltered.contains("api") && unfiltered.contains("docs"),
        "{unfiltered}"
    );
    assert!(!unfiltered.contains("reviewer"), "{unfiltered}");

    // The positive control: the name finds the session.
    let found = screen("api");
    assert!(found.contains("api"), "{found}");
    assert!(!found.contains("docs"), "api hides docs:\n{found}");
    assert!(!found.contains("nothing matches"), "{found}");

    for word in ["blocked", "working", "codex"] {
        let found = screen(word);
        assert!(
            found.contains(&format!("nothing matches \"{word}\"")),
            "{word} finds no header:\n{found}"
        );
    }
}

#[test]
fn every_session_becomes_a_space_row_with_its_worst_agent_state() {
    let (s, cfg) = two_workspaces();
    let snap = snapshot(&s.0, &cfg);

    let names: Vec<&str> = snap
        .spaces
        .iter()
        .map(|s| s.session_name.as_str())
        .collect();
    assert_eq!(names, vec!["api", "docs"], "list-sessions order");

    let api = &snap.spaces[0];
    assert_eq!(
        api.state,
        Some(AgentState::Blocked),
        "blocked beats working"
    );
    assert_eq!(api.agent_count, 2);
    assert!(api.active_pane_id.is_some(), "a jump and preview target");
    assert!(api.active_window_id.starts_with('@'));

    let docs = &snap.spaces[1];
    assert_eq!(docs.state, None, "no agents, no rolled-up state");
    assert_eq!(docs.agent_count, 0);
}

#[test]
fn a_workspace_in_a_repository_reports_its_branch() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-q", "-b", "trunk"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
    ] {
        let ok = taw::git::command()
            .arg("-C")
            .arg(&repo)
            .args(&args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }
    std::fs::write(repo.join("untracked.txt"), "hi").unwrap();
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "repo",
        "-c",
        repo.to_str().unwrap(),
    ])
    .unwrap();

    let cfg = Config::default();
    let snap = snapshot_waiting_for_git(&s.0, &cfg, Some("repo"));
    let row = snap
        .spaces
        .iter()
        .find(|s| s.session_name == "repo")
        .expect("the repo workspace");
    let info = row.git.as_ref().expect("git info for a work tree");
    assert_eq!(info.branch.as_deref(), Some("trunk"));
    assert_eq!(info.untracked, 1);
    assert_eq!(data::git_status_text(info), "+1");
}

#[test]
fn the_priority_sort_puts_the_blocked_agent_first_whatever_window_it_is_in() {
    let (s, _) = two_workspaces();
    let t = &s.0;
    // Give `docs` a *blocked* agent with the newest transition, so priority order
    // and workspace order genuinely disagree: it sorts last by workspace and first by
    // priority.
    let dotfiles_pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "docs")
        .unwrap()
        .pane_id;
    store::write_state(
        t,
        &dotfiles_pane,
        AgentState::Blocked,
        None,
        "manifest",
        900,
    )
    .unwrap();
    t.set_pane_option(&dotfiles_pane, "@agent_kind", "opencode")
        .unwrap();

    let cfg = Config::default();
    let snap = snapshot(t, &cfg);
    assert_eq!(snap.agents.len(), 3);

    // A flat list is what `agent_panel_sort` still orders; the tree keeps tmux's order.
    let agents = |sort| -> Vec<(String, u64)> {
        tree::build(
            &snap,
            &[],
            Depth::Agents,
            Layout::Flat,
            sort,
            true,
            true,
            &tree::Labels::default(),
            None,
        )
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Pane(pane) => snap.panes[*pane].agent,
            _ => None,
        })
        .map(|agent| {
            (
                snap.agents[agent].session_name.clone(),
                snap.agents[agent].state_since,
            )
        })
        .collect()
    };

    assert_eq!(
        agents(AgentPanelSort::Sessions),
        vec![
            ("api".to_string(), 100),
            ("api".to_string(), 300),
            ("docs".to_string(), 900)
        ],
        "workspace order: api's two windows, then docs"
    );
    assert_eq!(
        agents(AgentPanelSort::Attention),
        vec![
            ("docs".to_string(), 900),
            ("api".to_string(), 100),
            ("api".to_string(), 300)
        ],
        "priority order: both blocked agents first, newest transition first, then working"
    );
}

#[test]
fn a_custom_token_row_reads_the_agent_token_options() {
    let (s, _) = two_workspaces();
    let t = &s.0;
    let pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.agent.kind.as_deref() == Some("claude"))
        .unwrap()
        .pane_id;
    t.set_pane_option(&pane, "@agent_token_jj_status", "2 files")
        .unwrap();

    // The default rows use no `$token`, so nothing is read...
    let default = data::build(
        t,
        &Config::default(),
        &GitWorker::new(Duration::from_secs(2)),
        None,
        None,
    )
    .unwrap();
    assert!(default.agents.iter().all(|a| a.tokens.is_empty()));

    // ...and a config that asks for one gets it.
    let cfg: Config =
        toml::from_str("[ui.sidebar.agents]\nrows = [[\"workspace\", \"$jj_status\"]]\n").unwrap();
    let snap = data::build(t, &cfg, &GitWorker::new(Duration::from_secs(2)), None, None).unwrap();
    let row = snap
        .agents
        .iter()
        .find(|a| a.kind.as_deref() == Some("claude"))
        .unwrap();
    assert_eq!(
        row.tokens.get("jj_status").map(String::as_str),
        Some("2 files")
    );
}

#[test]
fn the_popup_opens_on_the_real_snapshot_and_jumps_to_the_pane_tmux_reported() {
    let (s, cfg) = two_workspaces();
    let snap = snapshot(&s.0, &cfg);
    // One agent is blocked and not yet looked at (tier 0) and the other is working
    // (tier 3), so the blocked one is the one row that needs a human.
    let expected = snap
        .agents
        .iter()
        .find(|a| a.state == Some(AgentState::Blocked))
        .expect("the watcher wrote a blocked agent")
        .clone();
    let theme = theme::resolve("catppuccin", &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);

    // Two sessions, their panes, and one row that needs a human.
    assert_eq!(
        app.rows
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Session(_)))
            .count(),
        2
    );
    assert_eq!(
        app.next_up_rows().len(),
        1,
        "only the blocked agent is walked"
    );
    assert_eq!(app.hot_count(), 1);
    let press = |app: &mut App, code: KeyCode| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    // The cursor opens on the pane the popup was opened from, which no
    // client of this test server is in, so `Tab` is what walks to the blocked agent.
    press(&mut app, KeyCode::Tab);
    assert_eq!(
        app.preview_pane().as_deref(),
        Some(expected.pane_id.as_str())
    );
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(taw::tui::actions::Action::Jump {
            session: expected.session_id.clone(),
            window: expected.window_id.clone(),
            pane: expected.pane_id.clone(),
        })
    );

    // The filter matches the row text tmux's own values produced: `@agent_name` is one of
    // them. Asserted on the matching row itself — a header also carries its session's
    // path, and a checkout directory can contain any word at all, so which *other* rows
    // survive a query is not this test's to predict.
    press(&mut app, KeyCode::Char('/'));
    for c in "'reviewer".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    let at = app
        .rows
        .rows
        .iter()
        .position(|row| match row {
            Row::Pane(pane) => app.snap.panes[*pane]
                .agent
                .is_some_and(|agent| app.snap.agents[agent].agent_label() == "reviewer"),
            _ => false,
        })
        .expect("the agent tmux named `reviewer` still has a row");
    assert!(
        app.rows.scores[at].is_some(),
        "and it matched the query on its own text, not inherited through its header"
    );
}

/// The popup's `r` -> `w` path ends in a real `rename-window`, not a toast.
///
/// `exec`'s worktree verbs are what `paths` and the binary path are for; a rename touches
/// neither, so `Paths::from_env` (pure path arithmetic, it creates nothing) is enough.
#[test]
fn renaming_a_window_runs_rename_window_against_tmux() {
    let (s, cfg) = two_workspaces();
    let t = &s.0;
    let window = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "api" && p.window_index == 1)
        .expect("api's second window")
        .window_id;

    let outcome = taw::tui::actions::exec(
        t,
        &cfg,
        &Paths::from_env(),
        std::path::Path::new("taw"),
        &taw::tui::actions::Action::RenameWindow {
            window: window.clone(),
            name: "planning".to_string(),
        },
        None,
    )
    .expect("rename-window must not take the popup down");

    assert_eq!(outcome.toast, None, "a rename that worked says nothing");
    assert!(!outcome.exit);
    let renamed = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.window_id == window)
        .expect("the window is still there")
        .window_name;
    assert_eq!(renamed, "planning");
}

/// A name that starts with a dash is the name the popup's rename gives tmux: tmux read it as a flag
/// (`unknown flag -planning`) unless a `--` ends the flags, and the popup showed that as a toast. A window
/// and a workspace, each renamed and read back from tmux.
#[test]
fn renaming_a_window_or_a_workspace_to_a_name_with_a_leading_dash_gives_tmux_that_name() {
    let (s, cfg) = two_workspaces();
    let t = &s.0;
    let pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "api" && p.window_index == 1)
        .expect("api's second window");
    let (window, session) = (pane.window_id.clone(), pane.session_id.clone());
    let rename = |action: taw::tui::actions::Action| {
        taw::tui::actions::exec(
            t,
            &cfg,
            &Paths::from_env(),
            std::path::Path::new("taw"),
            &action,
            None,
        )
        .expect("a rename must not take the popup down")
    };

    let outcome = rename(taw::tui::actions::Action::RenameWindow {
        window: window.clone(),
        name: "-planning".to_string(),
    });
    assert_eq!(outcome.toast, None, "a rename that worked says nothing");
    let outcome = rename(taw::tui::actions::Action::RenameWorkspace {
        session,
        name: "-renamed".to_string(),
    });
    assert_eq!(outcome.toast, None, "a rename that worked says nothing");

    let after = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.window_id == window)
        .expect("the window is still there");
    assert_eq!(after.window_name, "-planning");
    assert_eq!(after.session_name, "-renamed");
}

/// A rename tmux refuses is a toast carrying tmux's own reason, not a panic.
#[test]
fn a_rename_window_against_a_dead_target_reports_tmuxs_reason() {
    let (s, cfg) = two_workspaces();

    let outcome = taw::tui::actions::exec(
        &s.0,
        &cfg,
        &Paths::from_env(),
        std::path::Path::new("taw"),
        &taw::tui::actions::Action::RenameWindow {
            window: "@9999".to_string(),
            name: "planning".to_string(),
        },
        None,
    )
    .expect("a failed rename must not take the popup down");

    let toast = outcome.toast.expect("a refusal is reported");
    assert!(toast.contains("@9999"), "{toast}");
    assert!(
        !toast.contains("rename-window -t"),
        "no argv in a toast: {toast}"
    );
}
/// The pane the popup was opened from reaches `build` as an argument, not through the
/// environment: `display-popup -E` does not give its command the invoking pane's
/// environment, so `$TMUX_PANE` is unset inside the popup and `taw ui open` resolves the
/// pane *before* opening it (`--from`). Naming the second workspace's pane puts the
/// cursor's own row, and the `*` session, in that workspace.
///
/// Run with no origin, `build` answers `None` whether or not `cargo test` itself was run
/// from inside a tmux pane — which is the half that would fail if `build` still read the
/// environment behind the caller's back.
#[test]
fn build_takes_the_pane_the_popup_was_opened_from_as_an_argument() {
    let (s, cfg) = two_workspaces();
    let t = &s.0;
    let git = GitWorker::new(Duration::from_secs(2));
    let docs = t
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|session| session.session_name == "docs")
        .expect("the second workspace");
    let pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "docs")
        .expect("its one pane");

    let snap = data::build(t, &cfg, &git, Some(&pane.pane_id), None).unwrap();
    assert_eq!(snap.own_pane.as_deref(), Some(pane.pane_id.as_str()));
    assert_eq!(
        snap.current_session.as_deref(),
        Some(docs.session_id.as_str()),
        "the `*` session is the one holding the pane the popup came from"
    );

    let none = data::build(t, &cfg, &git, None, None).unwrap();
    assert_eq!(
        none.own_pane, None,
        "with no origin there is no own row — the environment is never consulted"
    );
}

/// `#{client_last_session}` is per client, so with two screens on one session each goes back
/// to its own place, and the popup reads the one it is on: the client its launcher named.
/// Picking a client from those attached takes the first the server lists, which reads the
/// other screen's place when the popup is on the second.
#[test]
fn the_previous_session_is_the_one_the_popups_own_client_came_from() {
    let s = Server::start();
    let t = &s.0;
    for name in ["left-from-first", "came-from-second"] {
        t.run_ok(&["new-session", "-d", "-s", name, "/bin/sh"])
            .unwrap();
    }
    let first = Client::attach_as(t, "left-from-first", "-first");
    let second = Client::attach_as(t, "came-from-second", "-second");
    // Both go to `api`: the first from one session, the second from the other.
    for client in [&first, &second] {
        t.run_ok(&["switch-client", "-c", &client.name, "-t", "api"])
            .unwrap();
    }
    let id_of = |name: &str| t.display(name, "#{session_id}").unwrap();
    let cfg = Config::default();
    let git = GitWorker::new(Duration::from_secs(2));

    let on_first = data::build(t, &cfg, &git, None, Some(&first.name)).unwrap();
    let on_second = data::build(t, &cfg, &git, None, Some(&second.name)).unwrap();

    assert_eq!(
        (on_first.previous_session, on_second.previous_session),
        (
            Some(id_of("left-from-first")),
            Some(id_of("came-from-second"))
        )
    );
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): the tests of this file that make a repository, run again
/// with each of the variables pointing into a decoy, pass and leave it byte for byte as it was
/// (tests/common/git_decoy.rs).
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &["a_workspace_in_a_repository_reports_its_branch"],
        "leave_the_one_gits_variables_point_at",
    );
}

// ------------------------------------------ the list is read from tmux in one call

/// The listing as JSON, to compare what two reads gave.
fn json<T: serde::Serialize>(rows: &[T]) -> String {
    serde_json::to_string(rows).unwrap()
}

/// One read of the list gives what the separate `list-panes`, `list-windows`, `list-sessions` and global option
/// reads give on the same server: several sessions, windows and panes, agent options, names that hold a tab and a
/// line break, a pane title that says a marker's words, and the Agents view option set and unset.
#[test]
fn one_read_gives_what_the_separate_reads_give() {
    let (s, _cfg) = two_workspaces();
    let t = &s.0;
    t.run_ok(&["new-window", "-t", "docs", "-n", "second"])
        .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "docs:second", "/bin/sh"])
        .unwrap();
    // Names that hold a tab and a line break, folded by tmux into one line each.
    t.run_ok(&["rename-window", "-t", "api:0", "--", "tab\there\nnext"])
        .unwrap();
    t.run_ok(&["rename-session", "-t", "docs", "--", "do\tcs\nx"])
        .unwrap();
    let panes = t.list_panes().unwrap();
    let a_pane = panes[0].pane_id.clone();
    t.run_ok(&[
        "select-pane",
        "-t",
        &a_pane,
        "-T",
        "taw: end of the window listing",
    ])
    .unwrap();
    t.set_pane_option(&a_pane, "@agent_display", "taw: end of the session listing")
        .unwrap();

    let compare = |view: Option<&str>| {
        match view {
            Some(value) => t
                .set_global_option(taw::tui::agent_view::OPTION, value)
                .unwrap(),
            None => {
                let _ = t.unset_global_option(taw::tui::agent_view::OPTION);
            }
        }
        let listing = t.read_listing(None).unwrap();
        assert_eq!(json(&listing.panes), json(&t.list_panes().unwrap()));
        assert_eq!(json(&listing.windows), json(&t.list_windows().unwrap()));
        assert_eq!(json(&listing.sessions), json(&t.list_sessions().unwrap()));
        for name in [
            taw::tui::agent_view::OPTION,
            "status",
            "default-shell",
            "@nothing",
        ] {
            assert_eq!(
                listing.option(name),
                t.get_global_option(name).unwrap(),
                "{name}"
            );
        }
        assert_eq!(
            listing.option(taw::tui::agent_view::OPTION).as_deref(),
            view
        );
        listing
    };
    let set = compare(Some("{\"rows\": \"a b\"}"));
    assert_eq!(set.sessions.len(), 2);
    assert!(set.panes.len() >= 4, "{:?}", set.panes.len());
    assert!(
        set.panes
            .iter()
            .any(|p| p.pane_title == "taw: end of the window listing")
    );
    assert!(set.windows.len() >= 4, "{}", set.windows.len());
    compare(None);
}

/// A tmux that logs each process it is started as and then is the real one, over the same server.
fn counting(t: &Tmux, dir: &std::path::Path) -> (Tmux, std::path::PathBuf) {
    let log = dir.join("calls");
    let script = dir.join("tmux-counting");
    common::write_script(
        &script,
        &format!(
            "echo \"$1 $2\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            t.bin.display()
        ),
    );
    (
        Tmux {
            bin: script,
            socket: t.socket.clone(),
        },
        log,
    )
}

/// The popup's build and the app's build each start tmux once, the popup's with its client asked in the same
/// call, and a client that is gone takes nothing of the list down. Five processes before.
#[test]
fn a_read_of_the_list_starts_tmux_once() {
    let (s, cfg) = two_workspaces();
    let dir = tempfile::tempdir().unwrap();
    let (t, log) = counting(&s.0, dir.path());
    let git = GitWorker::new(Duration::from_secs(2));
    let started = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count()
    };

    let app = data::build_without_client(&t, &cfg, &git, None).unwrap().0;
    assert_eq!(started(), 1, "the app's read");
    let plain = data::build(&s.0, &cfg, &git, None, None).unwrap();
    let names = |snap: &data::Snapshot| {
        snap.spaces
            .iter()
            .map(|space| space.session_name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&app),
        names(&plain),
        "the same list as the popup draws"
    );

    let before = started();
    let gone = data::build(&t, &cfg, &git, None, Some("/dev/ttys-gone")).unwrap();
    assert_eq!(
        started() - before,
        1,
        "the popup's read, its client asked in it"
    );
    assert_eq!(
        names(&gone),
        names(&plain),
        "a gone client takes none of the list down"
    );
    assert_eq!(
        gone.previous_session.is_some(),
        plain.previous_session.is_some()
    );
}

/// No server is the error `list-panes` gave, which the callers tell from the rest.
#[test]
fn a_read_of_the_list_with_no_server_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let t = Tmux {
        bin: Tmux::with_socket_name("unused").bin,
        socket: Some(taw::tmux::Socket::Path(dir.path().join("none.sock"))),
    };
    let err = t.read_listing(None).unwrap_err();
    assert!(taw::tmux::no_server(&err), "{err:#}");
}

/// Going in asks tmux once: the ids of the target and of the session came from come from one call on a real
/// server, the same ids the separate reads give, and a session gone on either side is told as before.
#[test]
fn the_ids_of_a_way_in_come_from_one_call() {
    let (s, _) = two_workspaces();
    let dir = tempfile::tempdir().unwrap();
    let (t, log) = counting(&s.0, dir.path());
    let id_of = |name: &str| {
        s.0.run(&[
            "display-message",
            "-p",
            "-t",
            &format!("={name}:"),
            "#{session_id}",
        ])
        .unwrap()
        .trim()
        .to_string()
    };
    let (api, docs) = (id_of("api"), id_of("docs"));
    let started = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let ids = |target: &str, from: Option<&str>| {
        let landed = taw::cli::open::landed(&t, target, from);
        (landed.target_id, landed.from_id)
    };
    let some = |id: &str| Some(id.to_string());

    assert_eq!(ids("=api", Some(&docs)), (some(&api), some(&docs)));
    assert_eq!(started(), 1, "both ids, one call");
    assert_eq!(ids(&docs, Some("=api")), (some(&docs), some(&api)));
    assert_eq!(ids("=api", None), (some(&api), None));
    assert_eq!(
        ids("=api", Some("$999")),
        (some(&api), None),
        "the session came from is gone, the target's id is still read"
    );
    assert_eq!(
        ids("=nobody", Some(&docs)),
        (None, None),
        "the target is gone"
    );
    assert_eq!(ids("$999", None), (None, None));
    assert_eq!(started(), 6);
}
