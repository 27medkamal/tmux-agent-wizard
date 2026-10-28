mod common;

use common::shell_env::ShellHome;
use std::path::PathBuf;

use taw::config::{Config, ToastDelivery};
use taw::notify::{self, Class, Notification, NotifiedAgent, SoundChoice};
use taw::paths::Paths;
use taw::rail;
use taw::store::{self, AgentState};
use taw::tmux::{Socket, Tmux};

/// A throwaway tmux server on its own `-L` socket, torn down on drop. Never touches the
/// user's real tmux server (no `-L`/`-S` means the default socket; this always sets one).
/// Copied from `tests/tmux_client.rs` — see its comments for why `-f /dev/null` and the
/// atomic counter (rather than just the pid) both matter.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        home.start_server(&t, &["-d", "-s", "one", "-x", "80", "-y", "24", "/bin/sh"]);
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

/// A real client attached to one of `inner`'s sessions.
///
/// tmux will not attach a client without a tty and a test process has none to give, so the
/// tty comes from a second throwaway server whose one pane runs `tmux -L <inner> attach`.
/// Dropping this kills that server, which detaches the client.
struct Client(Tmux, #[allow(dead_code)] ShellHome);

impl Client {
    /// Attaches to `session` and waits (bounded) for the inner server to see the client.
    fn attach(inner: &Tmux, session: &str) -> Client {
        let socket = match &inner.socket {
            Some(Socket::Name(name)) => name.clone(),
            other => panic!("the test server is always on a named socket, got {other:?}"),
        };
        let outer = Tmux::with_socket_name(&format!("{socket}-outer"));
        let home = ShellHome::new();
        let attach = format!("'{}' -L {socket} attach -t {session}", inner.bin.display());
        home.start_server(
            &outer,
            &["-d", "-s", "host", "-x", "80", "-y", "24", &attach],
        );
        for _ in 0..100 {
            if !inner.list_clients().unwrap_or_default().is_empty() {
                return Client(outer, home);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // `outer` is dropped here, taking its server with it.
        panic!("no client attached to {socket} within 5s");
    }

    /// The client's terminal reports gaining or losing focus, as a terminal does once tmux
    /// asks it to (`focus-events on`): `ESC [ I` or `ESC [ O`, typed into the client's tty.
    fn report_focus(&self, focused: bool) {
        let report = if focused { "49" } else { "4f" };
        self.0
            .run_ok(&["send-keys", "-t", "host", "-H", "1b", "5b", report])
            .unwrap();
    }
}

/// `#{client_flags}` of the one attached client: `focused` among them while tmux believes
/// its terminal has focus.
fn client_flags(t: &Tmux) -> String {
    t.run(&["list-clients", "-F", "#{client_flags}"]).unwrap()
}

/// Polls `ready` for up to 5 s.
fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// Which session the one attached client is looking at.
fn client_session(t: &Tmux) -> String {
    let clients = t.list_clients().unwrap();
    assert_eq!(clients.len(), 1, "one attached client: {clients:?}");
    clients[0].session_name.clone()
}

/// Minimal `Paths` rooted at a temp dir, for tests that need `deliver`'s
/// `paths.config_dir` but never actually touch sound files (every test here uses
/// `SoundChoice::None`, so `sound::play` returns before reading `config_dir` at all).
fn paths_for(dir: &std::path::Path) -> Paths {
    let agent_detection = dir.join("agent-detection");
    let plugins_dir = dir.join("plugins");
    Paths {
        config_dir: dir.to_path_buf(),
        config_file: dir.join("config.toml"),
        state_dir: dir.to_path_buf(),
        override_dir: agent_detection.clone(),
        remote_manifest_dir: agent_detection.join("remote"),
        manifest_marker: agent_detection.join("last-check"),
        sessions_dir: dir.join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: dir.join("logs"),
    }
}

// `deliver`, via `sound::play`, reads the process-global `TAW_SOUND_CMD` test seam
// (see `sound.rs`) in exactly one test below; guard it the same way `paths.rs`/
// `notify::system`'s own unit tests guard their env vars, in case another test that
// touches it is ever added to this file.
static SOUND_CMD_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct SoundCmdGuard<'a> {
    // Dropped after the variable is put back: the lock is held until then.
    _var: common::EnvVar,
    _lock: std::sync::MutexGuard<'a, ()>,
}

fn with_sound_cmd(script: &std::path::Path) -> SoundCmdGuard<'static> {
    let lock = SOUND_CMD_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SoundCmdGuard {
        _var: common::EnvVar::set("TAW_SOUND_CMD", script),
        _lock: lock,
    }
}

/// The same lock for `TAW_NOTIFY_CMD`, the desktop notifier's stand-in (see `notify::system`).
fn with_notify_cmd(script: &std::path::Path) -> SoundCmdGuard<'static> {
    let lock = SOUND_CMD_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SoundCmdGuard {
        _var: common::EnvVar::set("TAW_NOTIFY_CMD", script),
        _lock: lock,
    }
}

#[test]
fn is_background_is_true_for_every_pane_in_a_detached_session() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);

    // No client is attached to this throwaway server: even though this pane's window
    // is trivially "active" (it's the server's only window), the *session* is
    // detached (`session_attached == 0`), so is_background must still be true — see
    // its doc comment: a detached session is always background.
    assert!(s.0.list_clients().unwrap().is_empty());
    assert!(notify::is_background(&s.0, &p.pane_id).unwrap());
}

/// herdr asks one more thing before it calls a pane in front of the user: whether the
/// terminal it runs in has focus (`outer_terminal_focus`; an unfocused terminal is not
/// looking). tmux knows that of each client once `focus-events` is on: it asks the
/// terminal to report losing and regaining focus, and keeps the answer in
/// `#{client_flags}` (`focused`, tmux 3.6b). So the current window of an attached session
/// is background while its client's terminal is. With `focus-events` off tmux asks no
/// terminal, and a report that arrives anyway is not believed.
#[test]
fn is_background_is_true_while_the_attached_terminal_has_lost_focus() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    s.0.run_ok(&["set-option", "-g", "focus-events", "on"])
        .unwrap();
    let terminal = Client::attach(&s.0, "one");
    assert!(
        !notify::is_background(&s.0, &p.pane_id).unwrap(),
        "the pane is in front of the user"
    );

    terminal.report_focus(false);
    assert!(
        eventually(|| !client_flags(&s.0).contains("focused")),
        "tmux never took the focus-out report: {}",
        client_flags(&s.0)
    );
    assert!(
        notify::is_background(&s.0, &p.pane_id).unwrap(),
        "the user's terminal is in the background"
    );

    terminal.report_focus(true);
    assert!(
        eventually(|| client_flags(&s.0).contains("focused")),
        "tmux never took the focus-in report: {}",
        client_flags(&s.0)
    );
    assert!(!notify::is_background(&s.0, &p.pane_id).unwrap());

    s.0.run_ok(&["set-option", "-g", "focus-events", "off"])
        .unwrap();
    terminal.report_focus(false);
    assert!(
        eventually(|| !client_flags(&s.0).contains("focused")),
        "tmux never took the focus-out report: {}",
        client_flags(&s.0)
    );
    assert!(
        !notify::is_background(&s.0, &p.pane_id).unwrap(),
        "with focus-events off nobody's focus is known, so the pane is in front of them"
    );
}

#[test]
fn rail_refresh_writes_empty_string_with_no_agents_and_updates_after_a_transition() {
    let s = Server::start();
    let cfg = Config::default();

    let text = rail::refresh(&s.0, &cfg).unwrap();
    assert_eq!(text, "");
    assert_eq!(
        s.0.get_global_option("@agent_wizard_status").unwrap(),
        Some(String::new()),
        "refresh should have explicitly set the option to \"\", not left it unset"
    );

    let p = s.0.list_panes().unwrap().remove(0);
    store::write_state(
        &s.0,
        &p.pane_id,
        AgentState::Working,
        None,
        "manifest",
        1_700_000_000,
    )
    .unwrap();

    let text = rail::refresh(&s.0, &cfg).unwrap();
    assert!(text.contains('1'), "expected a count of 1 in {text:?}");
    assert_eq!(
        s.0.get_global_option("@agent_wizard_status").unwrap(),
        Some(text)
    );
}

#[test]
fn tmux_message_show_with_no_clients_is_a_noop_and_leaves_show_messages_working() {
    let s = Server::start();
    assert!(s.0.list_clients().unwrap().is_empty());

    notify::tmux_message::show(&s.0, "hello from taw").unwrap();

    // The server is left in a working state: show-messages (the server-wide message
    // log, readable even with no client attached) still succeeds afterward — show()
    // did not error out or leave anything broken despite having nobody to show to.
    s.0.run(&["show-messages"]).unwrap();
}

/// `notify::open_target`'s answer, with the group of the pane it went to spelled the way `taw
/// notification open-target` reports it in `target`.
fn open_target(t: &Tmux, focus: bool, after: Option<&str>) -> Option<(String, String)> {
    notify::open_target(t, focus, after)
        .unwrap()
        .map(|(pane, group)| {
            let group = serde_json::to_value(group).unwrap();
            (pane, group.as_str().unwrap().to_string())
        })
}

/// New windows in session `one`, each with a shell in it, and the panes they hold in
/// window order: the session's own window comes first, then one pane for each of `n`.
fn windows(s: &Server, n: usize) -> Vec<taw::tmux::PaneInfo> {
    for _ in 0..n {
        s.0.run_ok(&["new-window", "-t", "one", "/bin/sh"]).unwrap();
    }
    let mut panes = s.0.list_panes().unwrap();
    panes.sort_by_key(|p| p.window_index);
    panes
}

/// Makes `pane` an agent of kind `claude` in `state`, as the watcher would.
fn agent_in(t: &Tmux, pane: &str, state: Option<&str>) {
    t.set_pane_option(pane, "@agent_kind", "claude").unwrap();
    if let Some(state) = state {
        t.set_pane_option(pane, "@agent_state", state).unwrap();
    }
}

#[test]
fn open_target_returns_none_when_no_pane_holds_an_agent() {
    let s = Server::start();
    assert_eq!(open_target(&s.0, true, None), None);
    // A second window with a plain shell in it changes nothing.
    windows(&s, 1);
    assert_eq!(open_target(&s.0, true, None), None);
}

/// The `after`-driven walk end to end, on a real server: an explicit `after` reaches
/// `attention::next_up` unchanged (`Some` steps forward, wrapping); `None` reads the
/// session's own current pane and walks from there.
///
/// That `None` case is *not* the "no attached client -> the command fails" case other
/// tests in this file rely on for `focus: true`'s `switch-client`: `display-message -p`
/// with no `-t`/`-c` still resolves against whichever pane a client would land on if one
/// attached right now, which tmux tracks per session regardless of whether any client
/// actually is attached — confirmed by hand against this tmux build (`select-window`
/// alone, no attach, changes what a bare `display-message -p "#{pane_id}"` returns). So
/// the pane it names is whatever `select-window` last pointed at — including, in real
/// use, whatever the *previous* `open_target` call itself just selected: that is what
/// lets repeatedly pressing `prefix+O` (always `after: None`) step through every pane of
/// the walk in turn rather than re-landing on the same one every time.
#[test]
fn open_target_walks_between_two_blocked_panes_in_tmux_order() {
    let s = Server::start();
    let panes = windows(&s, 1);
    let (first, second) = (panes[0].clone(), panes[1].clone());
    agent_in(&s.0, &first.pane_id, Some("blocked"));
    agent_in(&s.0, &second.pane_id, Some("blocked"));

    // An explicit `after` is used as-is: the popup always has its own pane to pass, so
    // open_target must never consult tmux for one when the caller already supplied it.
    assert_eq!(
        open_target(&s.0, false, Some(&first.pane_id)),
        Some((second.pane_id.clone(), "needs_you".to_string())),
        "steps forward from the first blocked pane to the second"
    );
    assert_eq!(
        open_target(&s.0, false, Some(&second.pane_id)),
        Some((first.pane_id.clone(), "needs_you".to_string())),
        "wraps from the last blocked pane back to the first"
    );

    // Pin the session's current pane to a known value rather than trusting whatever the
    // two calls above happened to leave selected (their own successful select-pane is
    // exactly the same mechanism the None branch reads, so the point stands either way —
    // this just keeps the assertion below unambiguous).
    s.0.run_ok(&["select-window", "-t", &second.window_id])
        .unwrap();
    assert_eq!(
        s.0.display("one", "#{pane_id}").unwrap(),
        second.pane_id,
        "test setup: the session's current pane is the second one"
    );
    assert_eq!(
        open_target(&s.0, false, None),
        Some((first.pane_id.clone(), "needs_you".to_string())),
        "no explicit after: reads the session's current pane (second) and wraps to first"
    );
}

/// Nothing needs you, so every agent is the walk: the working agent
/// between the idle ones is a stop, and each press reports the group of the pane it went to.
#[test]
fn open_target_walks_every_agent_when_nothing_needs_you() {
    let s = Server::start();
    let panes = windows(&s, 3);
    let (idle_a, working, idle_b) = (panes[1].clone(), panes[2].clone(), panes[3].clone());
    agent_in(&s.0, &idle_a.pane_id, Some("idle"));
    agent_in(&s.0, &working.pane_id, Some("working"));
    agent_in(&s.0, &idle_b.pane_id, Some("idle"));
    let went =
        |pane: &taw::tmux::PaneInfo, group: &str| Some((pane.pane_id.clone(), group.to_string()));

    assert_eq!(
        open_target(&s.0, false, Some(&idle_a.pane_id)),
        went(&working, "rest"),
        "on to the working agent, and its group"
    );
    assert_eq!(
        open_target(&s.0, false, Some(&working.pane_id)),
        went(&idle_b, "idle")
    );
    assert_eq!(
        open_target(&s.0, false, Some(&idle_b.pane_id)),
        went(&idle_a, "idle"),
        "and round"
    );
    assert_eq!(
        open_target(&s.0, false, Some(&panes[0].pane_id)),
        went(&idle_a, "idle"),
        "from a plain shell before them all, the first"
    );
}

/// Nothing needs you and nothing is idle: the rest are the walk, working and unknown and an
/// agent that has reported no state yet alike.
#[test]
fn open_target_walks_the_rest_when_nothing_needs_you_and_nothing_is_idle() {
    let s = Server::start();
    let panes = windows(&s, 3);
    let (working, unknown, stateless) = (panes[1].clone(), panes[2].clone(), panes[3].clone());
    agent_in(&s.0, &working.pane_id, Some("working"));
    agent_in(&s.0, &unknown.pane_id, Some("unknown"));
    agent_in(&s.0, &stateless.pane_id, None);
    let rest = |pane: &taw::tmux::PaneInfo| Some((pane.pane_id.clone(), "rest".to_string()));

    assert_eq!(
        open_target(&s.0, false, Some(&working.pane_id)),
        rest(&unknown)
    );
    assert_eq!(
        open_target(&s.0, false, Some(&unknown.pane_id)),
        rest(&stateless)
    );
    assert_eq!(
        open_target(&s.0, false, Some(&stateless.pane_id)),
        rest(&working),
        "and round"
    );
}

/// A notification that has just been delivered does not steer the jump (one rule, next up,
/// so the latest notification does not go first). The agent that
/// finished last, in a later window, is the second stop of the walk, not the first: the
/// blocked one before it is.
#[test]
fn a_pending_notification_does_not_outrank_the_walk() {
    let s = Server::start();
    let panes = windows(&s, 2);
    let (shell, blocked, finished) = (panes[0].clone(), panes[1].clone(), panes[2].clone());
    agent_in(&s.0, &blocked.pane_id, Some("blocked"));
    agent_in(&s.0, &finished.pane_id, Some("done"));

    let cfg = Config::default();
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    let n = Notification {
        class: Class::Done,
        title: "claude done \u{b7} one:2".to_string(),
        body: "finished".to_string(),
        pane_id: Some(finished.pane_id.clone()),
        sound: SoundChoice::None, // never spawn a real audio player in tests
        agent: NotifiedAgent::default(),
    };
    notify::deliver(&s.0, &cfg, &paths, &n);

    assert_eq!(
        open_target(&s.0, true, Some(&shell.pane_id)),
        Some((blocked.pane_id.clone(), "needs_you".to_string())),
        "the walk goes to the first agent that needs you, not to the latest notification"
    );
    assert_eq!(
        open_target(&s.0, true, Some(&blocked.pane_id)),
        Some((finished.pane_id.clone(), "needs_you".to_string())),
        "and on to the finished one"
    );
}

/// A `[hooks]` table with `notification` set to `command`.
fn notification_hook(command: &str) -> taw::config::Hooks {
    toml::from_str::<Config>(&format!(
        "[hooks]\nnotification = {}\n",
        toml::Value::String(command.to_string())
    ))
    .unwrap()
    .hooks
}

/// A hook that writes its whole environment, and its input, where a test can read them once
/// they are complete: the environment file is renamed into place last.
fn recording_hook(out: &std::path::Path) -> String {
    format!(
        "cat > '{0}.json'; env | grep '^TAW_' | sort > '{0}.tmp' && mv '{0}.tmp' '{0}'",
        out.display()
    )
}

fn env_lines(out: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `announce` is `deliver` and then the `[hooks] notification` commands, so a hook is run
/// wherever a toast is shown, told the toast's own text, whether it is a request or a done,
/// and which pane it is about. The hook is a delivery of its own: `[ui.toast] delivery =
/// "off"` mutes the pop-up and, as it does for sounds, leaves the hook alone.
#[test]
fn announce_runs_the_notification_hook_with_the_toasts_text_and_its_pane_even_with_delivery_off() {
    for (class, kind, state, word) in [
        (Class::Request, "request", "blocked", "blocked"),
        (Class::Done, "done", "done", "done"),
    ] {
        let s = Server::start();
        let p = s.0.list_panes().unwrap().remove(0);
        agent_in(&s.0, &p.pane_id, Some(state));
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_for(tmp.path());
        let out = tmp.path().join("hook-env");
        let mut cfg = Config::default();
        cfg.ui.toast.delivery = ToastDelivery::Off;
        cfg.ui.sound.enabled = false;
        cfg.hooks = notification_hook(&recording_hook(&out));
        let n = Notification {
            class,
            title: format!("claude {word} \u{b7} one:0"),
            body: "needs input".to_string(),
            pane_id: Some(p.pane_id.clone()),
            sound: SoundChoice::None,
            agent: NotifiedAgent {
                label: "claude".to_string(),
                kind: "claude".to_string(),
            },
        };

        notify::announce(
            &s.0,
            &cfg,
            &paths,
            std::path::Path::new(env!("CARGO_BIN_EXE_taw")),
            &n,
        );

        assert!(eventually(|| out.exists()), "the hook never ran ({kind})");
        let env = env_lines(&out);
        for line in [
            "TAW_EVENT=notification".to_string(),
            format!("TAW_KIND={kind}"),
            format!("TAW_MESSAGE=claude {word} \u{b7} one:0 \u{b7} needs input"),
            "TAW_SESSION=one".to_string(),
            format!("TAW_PANE={}", p.pane_id),
            "TAW_AGENT=claude".to_string(),
            format!("TAW_STATE={state}"),
        ] {
            assert!(env.contains(&line), "{line} is missing from\n{env:#?}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.with_extension("json")).unwrap())
                .unwrap();
        assert_eq!(json["event"], "notification");
        assert_eq!(json["kind"], kind);
        assert_eq!(
            json["message"],
            format!("claude {word} \u{b7} one:0 \u{b7} needs input")
        );
        assert_eq!(json["pane"]["terminal_id"], p.pane_id.as_str());
    }
}

/// Runs `[hooks] notification` for `n` through `notification_from` with `machine`, on a server whose one pane
/// holds a blocked agent, and returns what the hook was given: its `TAW_*` environment and its JSON.
fn hook_told(
    machine: Option<&str>,
    about_the_local_pane: bool,
) -> (Vec<String>, serde_json::Value) {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    agent_in(&s.0, &p.pane_id, Some("blocked"));
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    let out = tmp.path().join("hook-env");
    let cfg = Config {
        hooks: notification_hook(&recording_hook(&out)),
        ..Config::default()
    };
    let n = Notification {
        class: Class::Request,
        title: "claude blocked \u{b7} garden:1".to_string(),
        body: "Do you want to proceed?".to_string(),
        pane_id: about_the_local_pane.then(|| p.pane_id.clone()),
        sound: SoundChoice::None,
        agent: NotifiedAgent {
            label: "claude".to_string(),
            kind: "claude".to_string(),
        },
    };
    let taw_bin = std::path::Path::new(env!("CARGO_BIN_EXE_taw"));
    let ctx = taw::plugin::PluginCtx {
        tmux: &s.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin,
        server_env: None,
    };

    assert_eq!(taw::hooks::notification_from(&ctx, &n, machine), 1);

    assert!(eventually(|| out.exists()), "the hook never ran");
    let json = serde_json::from_str(&std::fs::read_to_string(out.with_extension("json")).unwrap())
        .unwrap();
    (env_lines(&out), json)
}

/// `TAW_MACHINE` is empty for a notification no app told and for this Mac's own told through the app; a hook
/// about a machine is told its name, in the variable and in the JSON.
#[test]
fn a_notification_hook_is_told_which_machine_the_toast_is_about() {
    let (env, json) = hook_told(None, true);
    assert!(env.contains(&"TAW_MACHINE=".to_string()), "{env:#?}");
    assert!(
        env.contains(&"TAW_KIND=request".to_string()),
        "positive: it ran: {env:#?}"
    );
    assert!(json.get("machine").is_none(), "{json}");

    let (env, json) = hook_told(Some(""), true);
    assert!(env.contains(&"TAW_MACHINE=".to_string()), "{env:#?}");
    assert_eq!(json["machine"], "");
    assert!(
        json["pane"]["terminal_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{json}"
    );

    let (env, json) = hook_told(Some("studio"), false);
    assert!(env.contains(&"TAW_MACHINE=studio".to_string()), "{env:#?}");
    assert_eq!(json["machine"], "studio");
    assert_eq!(json["kind"], "request");
}

/// A toast about studio that names a pane number this Mac also has is not about this Mac's pane: the hook is
/// told none of this Mac's session, window, pane or agent state, and its JSON holds the machine's pane id
/// as the app names it. The same toast about this Mac (`Some("")`) finds the pane.
#[test]
fn a_toast_about_a_machine_never_finds_a_pane_of_this_one_with_the_same_id() {
    let (env, json) = hook_told(Some("studio"), true);

    assert!(env.contains(&"TAW_MACHINE=studio".to_string()), "{env:#?}");
    for line in [
        "TAW_PANE=",
        "TAW_SESSION=",
        "TAW_SESSION_ID=",
        "TAW_WINDOW=",
        "TAW_DIR=",
    ] {
        assert!(
            env.contains(&line.to_string()),
            "{line} expected in {env:#?}"
        );
    }
    assert!(
        json.get("pane")
            .is_none_or(|pane| pane.get("terminal_id").is_none()),
        "{json}"
    );
    assert_eq!(json["pane_id"], "studio:%0");
    assert_eq!(json["agent"], "claude");

    // The positive half: the same notification about this Mac's own pane finds it.
    let (env, _) = hook_told(Some(""), true);
    assert!(env.contains(&"TAW_PANE=%0".to_string()), "{env:#?}");
    assert!(env.contains(&"TAW_SESSION=one".to_string()), "{env:#?}");
}

/// A notification about nothing in particular (`taw notification show`) still reaches the
/// hook, with no pane, and with all of its text.
#[test]
fn announce_of_a_notification_about_no_pane_runs_the_hook_with_no_pane() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    let out = tmp.path().join("hook-env");
    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.hooks = notification_hook(&recording_hook(&out));
    let n = Notification {
        class: Class::Done,
        title: "Build finished".to_string(),
        body: "3 warnings".to_string(),
        pane_id: None,
        sound: SoundChoice::None,
        agent: NotifiedAgent::default(),
    };

    notify::announce(
        &s.0,
        &cfg,
        &paths,
        std::path::Path::new(env!("CARGO_BIN_EXE_taw")),
        &n,
    );

    assert!(eventually(|| out.exists()), "the hook never ran");
    let env = env_lines(&out);
    assert!(
        env.contains(&"TAW_MESSAGE=Build finished \u{b7} 3 warnings".to_string()),
        "{env:#?}"
    );
    assert!(env.contains(&"TAW_KIND=done".to_string()), "{env:#?}");
    assert!(env.contains(&"TAW_PANE=".to_string()), "{env:#?}");
}

/// With no hook configured `announce` is `deliver`: it asks tmux for nothing more.
#[test]
fn announce_with_no_notification_hook_starts_nothing_and_records_nothing_in_tmux() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    let n = Notification {
        class: Class::Done,
        title: "claude done \u{b7} one:0".to_string(),
        body: String::new(),
        pane_id: Some(p.pane_id.clone()),
        sound: SoundChoice::None,
        agent: NotifiedAgent::default(),
    };
    let before = global_user_options(&s.0);

    notify::announce(
        &s.0,
        &cfg,
        &paths,
        std::path::Path::new("/nonexistent/taw"),
        &n,
    );

    assert_eq!(global_user_options(&s.0), before);
    assert!(!paths.logs_dir.join("hooks.log").exists());
}

/// Every `@` option the server's global scope holds, one `name value` line each.
fn global_user_options(t: &Tmux) -> Vec<String> {
    t.run(&["show-options", "-g"])
        .unwrap()
        .lines()
        .filter(|line| line.starts_with('@'))
        .map(str::to_string)
        .collect()
}

/// `deliver` records nothing of the pane it notified about (`prefix+O` walks next up, not to
/// the last notification): a
/// delivered notification changes no `@` option on the server.
#[test]
fn deliver_leaves_no_record_of_the_pane_it_notified_about() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    // Default delivery is `herdr`: with zero attached clients, its display-message
    // loop simply runs zero times, exercised here as a safe no-op.
    let cfg = Config::default();
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());

    let n = Notification {
        class: Class::Done,
        title: "claude done \u{b7} one:0".to_string(),
        body: "finished".to_string(),
        pane_id: Some(p.pane_id.clone()),
        sound: SoundChoice::None, // never spawn a real audio player in tests
        agent: NotifiedAgent::default(),
    };
    let before = global_user_options(&s.0);

    notify::deliver(&s.0, &cfg, &paths, &n);

    assert_eq!(
        global_user_options(&s.0),
        before,
        "delivering a notification must record nothing in tmux"
    );
    assert_eq!(
        s.0.get_global_option("@agent_wizard_last_notified")
            .unwrap(),
        None,
        "and in particular not the pane it was about"
    );
}

/// `focus: false` is the popup's call, and it must leave every client where it is.
///
/// `switch-client` with no `-c` makes tmux pick the "current" client for itself, which
/// from inside a `display-popup` it cannot do reliably — it can move a client the user is
/// not looking at. The popup resolves its own client afterwards, so `open_target` must
/// only *select*: the target session's active window and pane change, nobody travels.
///
/// The bare form really would move this client, which is what makes the assertion mean
/// something: with one attached client on `two` and `$TMUX` unset, `switch-client -t one`
/// moves it. The second half of the test runs exactly that, after the first half has
/// proved `focus: false` did not.
#[test]
fn open_target_without_focus_selects_the_pane_but_moves_no_client() {
    let s = Server::start();
    // The agent lives in a second session; the client is looking at the first.
    s.0.run_ok(&["new-session", "-d", "-s", "agents", "/bin/sh"])
        .unwrap();
    s.0.run_ok(&["new-window", "-t", "agents", "/bin/sh"])
        .unwrap();
    let agent =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .find(|p| p.session_name == "agents" && p.window_index == 1)
            .expect("the agent's own window");
    s.0.set_pane_option(&agent.pane_id, "@agent_kind", "claude")
        .unwrap();

    let _client = Client::attach(&s.0, "one");
    assert_eq!(client_session(&s.0), "one", "the client starts on `one`");

    let jumped = open_target(&s.0, false, None);

    assert_eq!(
        jumped,
        Some((agent.pane_id.clone(), "rest".to_string())),
        "an agent that has reported no state is the rest, and the only agent there is"
    );
    assert_eq!(
        client_session(&s.0),
        "one",
        "the popup's call moved a client"
    );
    // It still did the half the popup needs: the agent's window is now the one its own
    // session is on, so the popup's `switch-client -c` lands the user on that pane.
    assert_eq!(
        s.0.display("agents", "#{window_id}").unwrap(),
        agent.window_id,
        "the agent's window was not selected"
    );
    assert_eq!(
        s.0.display("agents", "#{pane_id}").unwrap(),
        agent.pane_id,
        "the agent's pane was not selected"
    );

    // The control: the command `focus: false` skipped would have moved this client.
    s.0.run_ok(&["switch-client", "-t", "agents"]).unwrap();
    assert_eq!(
        client_session(&s.0),
        "agents",
        "a bare switch-client does move this client, so the assertion above is not vacuous"
    );
}

/// A pane-less `notification show` renders as a plain title/body toast — not forced into
/// the agent-transition shape, which used the title as an agent name, showed an empty
/// `<session>:<window>` location, and offered a `prefix+O next up` hint that goes nowhere.
#[test]
fn a_pane_less_notification_renders_plainly_with_no_jump_hint_or_lifecycle_label() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());

    // A real attached client, so the `herdr` toast path actually issues a display-message
    // call (see `AttachedClient`).
    let _client = AttachedClient::attach(&s);
    let before = s.0.run(&["show-messages"]).unwrap();

    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Herdr;
    // `Class::Request` would have rendered the "⏸ blocked" lifecycle label under the bug.
    let n = Notification {
        class: Class::Request,
        title: "Build finished".to_string(),
        body: "3 warnings".to_string(),
        pane_id: None,
        sound: SoundChoice::None,
        agent: NotifiedAgent::default(),
    };
    notify::deliver(&s.0, &cfg, &paths, &n);

    // `show-messages` prints newest-first; entries added since `before` are a prefix.
    let after = s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let new_lines: Vec<&str> = after.lines().take(new_count).collect();
    let logged = new_lines.join("\n");

    assert!(
        logged.contains("display-message"),
        "the toast path never issued a display-message: {new_lines:?}"
    );
    assert!(logged.contains("Build finished"), "{logged}");
    assert!(logged.contains("3 warnings"), "{logged}");
    // None of the agent-transition framing the bug wrongly applied.
    assert!(
        !logged.contains("next up"),
        "manual toast must not offer a hint about the key: {logged}"
    );
    assert!(
        !logged.contains(" blocked ") && !logged.contains(" done "),
        "manual toast must not carry a lifecycle label: {logged}"
    );
}

/// A pane-less notification whose body has nothing visible in it (`--body ' '`) ends after
/// its title, as one with no body does: no separator with nothing after it.
#[test]
fn a_pane_less_notification_with_a_blank_body_ends_after_its_title() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());

    // A real attached client, so the `herdr` toast path actually issues a display-message
    // call (see `AttachedClient`).
    let _client = AttachedClient::attach(&s);
    let before = s.0.run(&["show-messages"]).unwrap();

    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Herdr;
    let n = Notification {
        class: Class::Done,
        title: "Build finished".to_string(),
        body: "  ".to_string(),
        pane_id: None,
        sound: SoundChoice::None,
        agent: NotifiedAgent::default(),
    };
    notify::deliver(&s.0, &cfg, &paths, &n);

    // `show-messages` prints newest-first; entries added since `before` are a prefix.
    let after = s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let new_lines: Vec<&str> = after.lines().take(new_count).collect();
    let logged = new_lines.join("\n");

    assert!(
        logged.contains("display-message -c"),
        "the toast path never issued a display-message: {new_lines:?}"
    );
    assert!(logged.contains("Build finished"), "{logged}");
    assert!(
        !logged.contains('\u{b7}'),
        "a toast with no body has no separator after its title: {logged}"
    );
}

/// A finished agent whose pane has nothing to say (no line its rule matched, no pane
/// title) is delivered with an empty body. Its toast ends after the location, on the two
/// spaces before the hint, not on a separator with nothing after it.
#[test]
fn a_toast_about_a_pane_with_nothing_to_say_has_no_dangling_separator() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    s.0.set_pane_option(&p.pane_id, "@agent_kind", "claude")
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());

    // A real attached client, so the `herdr` toast path actually issues a display-message
    // call (see `AttachedClient`).
    let _client = AttachedClient::attach(&s);
    let before = s.0.run(&["show-messages"]).unwrap();

    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Herdr;
    let n = Notification {
        sound: SoundChoice::None, // never spawn a real audio player in tests
        ..notify::for_transition(
            &NotifiedAgent {
                label: "claude".to_string(),
                kind: "claude".to_string(),
            },
            "one",
            0,
            Class::Done,
            "",
            &p.pane_id,
            "O",
        )
    };
    notify::deliver(&s.0, &cfg, &paths, &n);

    // `show-messages` prints newest-first; entries added since `before` are a prefix.
    let after = s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let new_lines: Vec<&str> = after.lines().take(new_count).collect();
    let logged = new_lines.join("\n");

    assert!(
        logged.contains("display-message -c"),
        "the toast path never issued a display-message: {new_lines:?}"
    );
    assert!(
        logged.contains("claude \u{b7} one:0  #[fg=brightblack]prefix+O next up#[default]"),
        "the toast should end after its location, then the hint: {logged}"
    );
}

#[test]
fn deliver_with_terminal_delivery_and_no_clients_is_a_safe_noop() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Terminal;
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());

    let n = notify::for_transition(
        &NotifiedAgent {
            label: "claude".to_string(),
            kind: "claude".to_string(),
        },
        "one",
        0,
        Class::Request,
        "blocked on input",
        &p.pane_id,
        "O",
    );
    assert_eq!(n.sound, SoundChoice::Request);
    // Override to None so this test never spawns a real audio player.
    let n = Notification {
        sound: SoundChoice::None,
        ..n
    };
    let options_before = global_user_options(&s.0);
    let before = s.0.run(&["show-messages"]).unwrap();

    notify::deliver(&s.0, &cfg, &paths, &n);

    // With no client to write the sequence to, nothing is shown by other means either:
    // tmux's message log records one `display-message` line for each one that was issued.
    // `show-messages` prints newest-first, so entries added since `before` are a prefix.
    let after = s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let new_lines: Vec<&str> = after.lines().take(new_count).collect();
    assert!(
        !new_lines.iter().any(|l| l.contains("display-message")),
        "terminal delivery with no clients must not fall back to display-message: {new_lines:?}"
    );
    assert_eq!(
        global_user_options(&s.0),
        options_before,
        "and it records nothing in tmux either"
    );
}

#[test]
fn live_toast_context_is_not_corrupted_when_names_contain_the_title_marker_text() {
    let s = Server::start();
    // Both halves `for_transition` embeds in `title` around the literal
    // marker text (" blocked \u{b7} "/" done \u{b7} ") are independently
    // user-controlled, with nothing at this layer stopping either from containing
    // it: the session name via plain tmux `rename-session` (no charset restriction
    // at all), and the display name via `@agent_display` (`pane report-metadata
    // --display-agent`, also unrestricted). Recovering
    // kind_or_name/location by splitting `title` back apart on that marker text
    // would silently corrupt either case; querying tmux live
    // (`live_toast_context`) has no such failure mode.
    s.0.run_ok(&[
        "rename-session",
        "-t",
        "one",
        "sneaky blocked \u{b7} session",
    ])
    .unwrap();
    let p = s.0.list_panes().unwrap().remove(0);
    let tricky_display = "sneaky done \u{b7} display";
    s.0.set_pane_option(&p.pane_id, "@agent_display", tricky_display)
        .unwrap();

    let (kind_or_name, location) = notify::live_toast_context(&s.0, &p.pane_id).unwrap();

    assert_eq!(kind_or_name, tricky_display);
    assert_eq!(
        location,
        format!("sneaky blocked \u{b7} session:{}", p.window_index)
    );

    // And the full rendered toast still embeds both verbatim — not truncated,
    // misplaced, or merged with the surrounding template text by the tricky names.
    let text = notify::tmux_message::format_message(
        Class::Request,
        &kind_or_name,
        &location,
        "detail",
        "O",
    );
    assert!(text.contains(tricky_display), "{text}");
    assert!(text.contains("sneaky blocked \u{b7} session"), "{text}");
}

#[test]
fn deliver_with_off_delivery_skips_the_toast_but_still_plays_sound() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Off;

    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());

    // Sound test seam (TAW_SOUND_CMD, see sound.rs): the script just touches a
    // marker file, so its existence proves sound::play reached a genuine spawn
    // rather than an early return — without playing real audio or depending on
    // afplay/paplay/aplay being installed on whatever machine runs this test.
    let marker = tmp.path().join("sound-played");
    let script = tmp.path().join("record-sound.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    crate::common::make_executable(&script);
    cfg.ui.sound.enabled = true;
    cfg.ui.sound.request_path = Some(PathBuf::from("whatever.wav"));

    let before = s.0.run(&["show-messages"]).unwrap();

    let n = Notification {
        class: Class::Request,
        title: "claude blocked \u{b7} one:0".to_string(),
        body: "needs input".to_string(),
        pane_id: Some(p.pane_id.clone()),
        sound: SoundChoice::Request,
        agent: NotifiedAgent::default(),
    };
    {
        let _guard = with_sound_cmd(&script);
        notify::deliver(&s.0, &cfg, &paths, &n);
    }

    // Sound is still attempted under Off (herdr's `off` only disables the pop-up):
    // wait for the detached, reaped child to run and write its marker. The budget is ten
    // seconds rather than two because this test spawns a process on a machine already
    // running a tmux server and the rest of the suite, where two seconds is a coin toss
    // rather than a limit; the loop still exits the instant the marker appears, so the
    // budget costs nothing when nothing is wrong.
    let mut waited = std::time::Duration::ZERO;
    let step = std::time::Duration::from_millis(20);
    while !marker.exists() && waited < std::time::Duration::from_secs(10) {
        std::thread::sleep(step);
        waited += step;
    }
    assert!(
        marker.exists(),
        "sound::play should still spawn under Off delivery"
    );

    // No display-message call was issued: Off's branch in show_toast (see
    // notify/mod.rs) returns before any tmux command runs at all. Every `Tmux::run`
    // call is its own short-lived `tmux <args>` client process, and tmux's message
    // log (`show-messages`) records one "... command: <args>" line per such client —
    // so if Off had (incorrectly) shown a toast, a `display-message` line would show
    // up among the entries added since `before`. `show-messages` prints newest-first,
    // so those are a prefix of `after`.
    let after = s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let new_lines: Vec<&str> = after.lines().take(new_count).collect();
    assert!(
        !new_lines.iter().any(|l| l.contains("display-message")),
        "Off delivery must never issue display-message: {new_lines:?}"
    );
}

/// Holds a real attached control-mode client on `s`'s server, so `tmux_message::show`'s
/// `list_clients()` loop actually has somewhere to send a `display-message -c
/// <client_name> <text>` to. With zero attached clients, `show()` is a documented no-op
/// (see `tmux_message_show_with_no_clients_is_a_noop...` above): the herdr toast path
/// would never reach tmux's format parser at all, and a test built on that would pass
/// whether or not the escaping fix below is actually present — the whole point of
/// attaching a real client here is to rule that out.
struct AttachedClient(std::process::Child);

impl AttachedClient {
    /// Attaches in control mode (`-C`), which needs no tty: the client stays attached
    /// for as long as its stdin stays open (held here, closed by `Drop::drop` killing
    /// the child). Mirrors `tests/cli_json.rs`'s `AttachedClient`.
    fn attach(s: &Server) -> AttachedClient {
        let socket_name = match &s.0.socket {
            Some(taw::tmux::Socket::Name(name)) => name.clone(),
            other => panic!("expected Server to use a named socket, got {other:?}"),
        };
        let child = std::process::Command::new(&s.0.bin)
            .arg("-L")
            .arg(&socket_name)
            .args(["-C", "attach-session", "-t", "one"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn a control-mode tmux client");
        // Owned before the polling below, so `Drop` reaps the child even if that panics.
        let client = AttachedClient(child);

        // Attaching is asynchronous; wait for the server to actually report the client.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if matches!(s.0.list_clients(), Ok(clients) if !clients.is_empty()) {
                return client;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no tmux client attached within 5s");
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether `s` contains a `#(` that tmux's format parser would still treat as live job
/// substitution: a run of consecutive `#` characters of *odd* length immediately
/// followed by `(`. tmux consumes a doubled `##` as one literal `#` and resumes
/// scanning past it, so only an odd run leaves a final, unescaped `#` free to start a
/// `#(...)` sequence. Copied from the equivalent (private, other-crate) helper in
/// `notify::tmux_message`'s own unit tests — see its doc comment for the full
/// reasoning, in particular for why naive substring search (`s.contains("#(")`) cannot
/// tell escaped output from unescaped: doubling a lone `#` before `(` always yields a
/// string that still "contains" `#(` as a substring, starting at the second `#`.
fn has_unescaped_hash_paren(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            let run = i - start;
            if run % 2 == 1 && bytes.get(i) == Some(&b'(') {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

/// The security test for the toast text: `@agent_display` (the tmux pane option `taw
/// pane report-metadata --display-agent`/`taw agent rename` write, and exactly what
/// `notify::live_toast_context` reads for a herdr toast's `kind_or_name`) is set to a
/// tmux `#(...)` job-substitution payload, which `notify::tmux_message` has to escape
/// (`escape_format`) before it goes into the `display-message` text. This drives the real
/// `notify::deliver` -> `show_toast` -> `tmux_message::show` path end to end against a
/// live tmux server.
///
/// Two things are checked, for two different reasons:
///
/// * The tempfile is never created: the property that has to hold, and true whatever
///   `escape_format` does. Empirically (verified by hand against this exact tmux 3.6b
///   binary, across `-p`, `-c` with a real attached pty client, and a self-backgrounding
///   `&` payload, none of them ran the job) `display-message` does not appear to run
///   `#(...)` jobs at all, unlike a redrawn `status-left` (confirmed as a positive
///   control) or `pipe-pane`'s command (guarded separately by `watch::pipe_command_word`,
///   see its doc comment). So this assertion alone would pass even without the escaping
///   on this tmux build: it is defense in depth, not proof of the escaping.
/// * `show-messages`' log line for the `display-message` call, which echoes the raw
///   argv tmux received, *not* a format-expanded result, contains no unescaped `#(`.
///   This is the assertion that tells escaped from unescaped: without the escaping,
///   `kind_or_name` reaches that argv as a bare `#(touch ...)` and this fails (shown by
///   removing the `escape_format(kind_or_name)` call in
///   `notify::tmux_message::format_message`, which fails this test while the tempfile
///   assertion above still passes).
///
/// `%` strftime substitution, by contrast, *is* live and synchronous in
/// `display-message` on this tmux build (also verified by hand: an unescaped `%d`
/// becomes today's day-of-month, `%n` becomes a real newline), so that half is
/// exercised by the unit tests in `notify::tmux_message` instead
/// (`escapes_percent_strftime_sequence_in_location` et al.), where a live tmux server
/// isn't needed to observe it.
#[test]
fn herdr_toast_never_leaks_unescaped_job_substitution_from_an_attacker_controlled_agent_display() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    let pwned = tmp.path().join("pwned");

    s.0.set_pane_option(
        &p.pane_id,
        "@agent_display",
        &format!("#(touch {})", pwned.display()),
    )
    .unwrap();

    // A real attached client, so the toast path actually issues a display-message call
    // — see `AttachedClient`'s doc comment for why this matters.
    let _client = AttachedClient::attach(&s);

    let before = s.0.run(&["show-messages"]).unwrap();

    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Herdr;
    let n = Notification {
        class: Class::Request,
        title: "placeholder blocked \u{b7} one:0".to_string(),
        body: "needs input".to_string(),
        pane_id: Some(p.pane_id.clone()),
        sound: SoundChoice::None, // never spawn a real audio player in tests
        agent: NotifiedAgent::default(),
    };
    notify::deliver(&s.0, &cfg, &paths, &n);

    // Belt-and-braces: give tmux a moment in case `#(...)` job execution ever does fire
    // for this code path on some other tmux build.
    std::thread::sleep(std::time::Duration::from_secs(1));

    assert!(
        !pwned.exists(),
        "tmux must never execute shell substitution from an agent_display value"
    );

    // `show-messages` prints newest-first (verified against tmux 3.6b: each call's own
    // "command: show-messages" entry appears at the *top* of its own output) — unlike
    // an append-only log, entries added between `before` and `after` are a *prefix* of
    // `after_lines`, not a suffix.
    let after = s.0.run(&["show-messages"]).unwrap();
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let new_count = after_lines.len().saturating_sub(before_lines.len());
    let new_lines = &after_lines[..new_count];

    // Sanity check that the toast path actually ran and carried the payload through to
    // tmux, rather than every assertion below passing vacuously because
    // display-message was never called (e.g. `show()` silently found no attached
    // clients).
    assert!(
        new_lines.iter().any(|l| l.contains("display-message")),
        "expected a display-message call to be logged: {new_lines:?}"
    );
    assert!(
        new_lines.iter().any(|l| l.contains("#(touch")),
        "expected the (escaped) payload text to reach tmux's message log: {new_lines:?}"
    );

    // The check that matters: the logged argv must never contain a live, unescaped
    // `#(`. See this test's doc comment for why this (not the tempfile check above) is
    // what tells escaped from unescaped.
    let joined = new_lines.join("\n");
    assert!(
        !has_unescaped_hash_paren(&joined),
        "an unescaped #( reached tmux's command log — kind_or_name is not being \
         escaped: {new_lines:?}"
    );
}

#[test]
fn live_toast_context_is_not_shifted_by_a_tab_inside_agent_display() {
    // `live_toast_context` reads five columns from one tab-joined `display-message` and
    // splits the reply on tabs. Four of the five are free text, and `@agent_display` is a
    // pane option — which, unlike a session or window *name* (tmux escapes a tab there),
    // round-trips a literal tab verbatim. One such tab would shift `@agent_name` into the
    // `@agent_kind` slot, so the toast's label would be the wrong half of someone's display
    // name. `tmux::fold_controls` folds each column server-side.
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);

    s.0.set_pane_option(&p.pane_id, "@agent_display", "Review\tBot")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_name", "reviewer")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_kind", "claude")
        .unwrap();

    // The hazard is real: tmux replays the pane option's tab.
    assert_eq!(
        s.0.display(&p.pane_id, "#{@agent_display}").unwrap(),
        "Review\tBot"
    );

    let (kind_or_name, location) = notify::live_toast_context(&s.0, &p.pane_id).unwrap();

    assert_eq!(
        kind_or_name, "Review Bot",
        "the display name must arrive whole, with its tab folded to a space"
    );
    assert_eq!(location, format!("one:{}", p.window_index));

    // A newline is the worse half — it would end the reply's only line partway through.
    s.0.set_pane_option(&p.pane_id, "@agent_display", "Review\nBot")
        .unwrap();
    let (kind_or_name, location) = notify::live_toast_context(&s.0, &p.pane_id).unwrap();
    assert_eq!(kind_or_name, "Review Bot");
    assert_eq!(location, format!("one:{}", p.window_index));
}

/// An agent that exits mid-turn is toasted done after the watcher has cleared its record
/// (herdr's exit report), so its pane names no agent by the time the toast is delivered.
/// The `herdr` toast then names the agent the notification is about, as herdr's does,
/// rather than nobody.
#[test]
fn a_toast_about_an_agent_whose_record_was_cleared_names_that_agent() {
    let s = Server::start();
    // No `@agent_*` option on it: what the clear leaves.
    let p = s.0.list_panes().unwrap().remove(0);
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    // A real attached client, so the `herdr` toast path issues a display-message call.
    let _client = AttachedClient::attach(&s);
    let before = s.0.run(&["show-messages"]).unwrap();

    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Herdr;
    let n = Notification {
        sound: SoundChoice::None, // never spawn a real audio player in tests
        ..notify::for_transition(
            &NotifiedAgent {
                label: "reviewer".to_string(),
                kind: "claude".to_string(),
            },
            "one",
            0,
            Class::Done,
            "finished",
            &p.pane_id,
            "O",
        )
    };
    notify::deliver(&s.0, &cfg, &paths, &n);

    // `show-messages` prints newest-first; entries added since `before` are a prefix.
    let after = s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let logged = after.lines().take(new_count).collect::<Vec<_>>().join("\n");
    assert!(
        logged.contains("display-message"),
        "the toast path never issued a display-message: {logged}"
    );
    assert!(
        logged.contains("done#[default] reviewer \u{b7} one:0"),
        "the toast must name the agent it is about: {logged}"
    );
}

/// The same for the sound: `[ui.sound.agents]` mutes a kind (herdr's default mutes
/// `droid`), and a droid that exited mid-turn stays muted once its record is cleared.
#[test]
fn an_agent_muted_by_kind_stays_muted_once_its_record_is_cleared() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_for(tmp.path());
    let played = tmp.path().join("played.txt");
    let script = tmp.path().join("record-sound.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho played >> {}\n", played.display()),
    )
    .unwrap();
    crate::common::make_executable(&script);

    let mut cfg = Config::default();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.ui.sound.enabled = true;
    cfg.ui.sound.done_path = Some(PathBuf::from("done.wav"));
    let about = |kind: &str| {
        notify::for_transition(
            &NotifiedAgent {
                label: kind.to_string(),
                kind: kind.to_string(),
            },
            "one",
            0,
            Class::Done,
            "finished",
            &p.pane_id,
            "O",
        )
    };
    {
        let _guard = with_sound_cmd(&script);
        // The muted droid first, then a Claude, which plays: once Claude's sound is
        // recorded, the droid's would have been too.
        notify::deliver(&s.0, &cfg, &paths, &about("droid"));
        notify::deliver(&s.0, &cfg, &paths, &about("claude"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !played.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    assert_eq!(
        std::fs::read_to_string(&played)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "only Claude's done may play; the droid is muted by its kind"
    );
}

/// What one `announce` of a blocked claude in pane `%N` of session `one` is given, with the sound and the hook
/// of a test: scripts that add one line to their own log and then fail, as a stand-in that is refused would.
struct Announcing {
    s: Server,
    _client: Client,
    tmp: tempfile::TempDir,
    paths: Paths,
    cfg: Config,
    n: Notification,
    hook_log: PathBuf,
    sound_log: PathBuf,
    sound_script: PathBuf,
}

impl Announcing {
    fn new(delivery: ToastDelivery) -> Announcing {
        let s = Server::start();
        let p = s.0.list_panes().unwrap().remove(0);
        agent_in(&s.0, &p.pane_id, Some("blocked"));
        let client = Client::attach(&s.0, "one");
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_for(tmp.path());
        let hook_log = tmp.path().join("hook.log");
        let sound_log = tmp.path().join("sound.log");
        let sound_script = tmp.path().join("record-sound.sh");
        std::fs::write(
            &sound_script,
            format!(
                "#!/bin/sh\necho played >> '{}'\nexit 1\n",
                sound_log.display()
            ),
        )
        .unwrap();
        crate::common::make_executable(&sound_script);
        let mut cfg = Config::default();
        cfg.ui.toast.delivery = delivery;
        cfg.ui.sound.enabled = true;
        cfg.ui.sound.request_path = Some(PathBuf::from("whatever.wav"));
        cfg.hooks = notification_hook(&format!(
            "echo \"$TAW_MESSAGE\" >> '{}'; exit 1",
            hook_log.display()
        ));
        let n = notify::for_transition(
            &NotifiedAgent {
                label: "claude".to_string(),
                kind: "claude".to_string(),
            },
            "one",
            0,
            Class::Request,
            "needs input",
            &p.pane_id,
            "O",
        );
        Announcing {
            s,
            _client: client,
            tmp,
            paths,
            cfg,
            n,
            hook_log,
            sound_log,
            sound_script,
        }
    }

    /// The lease on this machine, last refreshed `age_ms` ago.
    fn lease(&self, age_ms: u64) {
        let at = taw::machine::files::now_ms() - age_ms;
        assert!(taw::machine::files::offer_lease(&self.paths, "mac-1", at));
    }

    /// Announces, with the sound player stood in for, and returns what tmux logged for the toast.
    fn announce(&self) -> String {
        let before = self.s.0.run(&["show-messages"]).unwrap();
        let _guard = with_sound_cmd(&self.sound_script);
        notify::announce(
            &self.s.0,
            &self.cfg,
            &self.paths,
            std::path::Path::new(env!("CARGO_BIN_EXE_taw")),
            &self.n,
        );
        let after = self.s.0.run(&["show-messages"]).unwrap();
        let new_count = after.lines().count().saturating_sub(before.lines().count());
        after.lines().take(new_count).collect::<Vec<_>>().join("\n")
    }

    /// Waits until the hook and the sound have each logged `count` lines, then a moment more.
    fn wait_for_hook_and_sound(&self, count: usize) {
        let lines = |log: &PathBuf| {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .count()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while (lines(&self.hook_log) < count || lines(&self.sound_log) < count)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            (lines(&self.hook_log), lines(&self.sound_log)),
            (count, count),
            "hook lines and sound lines in {}",
            self.tmp.path().display()
        );
    }

    fn events(&self) -> Vec<taw::machine::wire::Down> {
        std::fs::read_to_string(self.paths.events_file())
            .unwrap_or_default()
            .lines()
            .map(|line| taw::machine::wire::parse_down(line).unwrap())
            .collect()
    }
}

/// With a fresh lease on this machine the app on a Mac is the one that speaks: the toast is shown here as
/// today, one `toast` line is left in the events file for the app (the lease flag false: the feed sets it),
/// and neither the sound nor the hook runs. A lease last refreshed 16 s ago is no lease: the same announce
/// plays the sound and runs the hook, and leaves no line.
#[test]
fn a_fresh_lease_shows_the_toast_here_tells_the_app_and_leaves_sound_and_hook_to_it() {
    let a = Announcing::new(ToastDelivery::Herdr);

    a.lease(1_000);
    let logged = a.announce();
    assert!(logged.contains("display-message"), "{logged}");
    assert!(logged.contains("needs input"), "{logged}");
    assert!(logged.contains("one:0"), "{logged}");
    let events = a.events();
    assert_eq!(events.len(), 1, "{events:#?}");
    let taw::machine::wire::Down::Toast(toast) = &events[0] else {
        panic!("not a toast: {events:#?}");
    };
    assert_eq!(
        toast,
        &taw::machine::wire::Toast {
            kind: "request".into(),
            pane: a.n.pane_id.clone().unwrap(),
            text: notify::plain_text(&a.n),
            title: a.n.title.clone(),
            body: "needs input".into(),
            agent: "claude".into(),
            agent_kind: "claude".into(),
            location: "one:0".into(),
            lease: false,
        }
    );

    // The positive half before the negative: with the lease old the same announce runs both, once each, so
    // the first announce ran neither.
    a.lease(16_000);
    a.announce();
    a.wait_for_hook_and_sound(1);
    assert_eq!(a.events().len(), 1, "a stale lease adds no line");
}

/// `[ui.toast] delivery = "off"` shows no toast under a fresh lease either, and the line for the app is
/// written all the same: the app has its own `[ui.toast]`.
#[test]
fn a_fresh_lease_with_delivery_off_writes_the_line_and_shows_nothing_here() {
    let a = Announcing::new(ToastDelivery::Off);
    a.lease(1_000);
    let logged = a.announce();
    assert_eq!(a.events().len(), 1, "positive: the line is written");
    // The line for the app reads the pane with display-message -p; a toast is the one that carries the words.
    assert!(!logged.contains("needs input"), "{logged}");
}

/// With no lease on file `announce` is what it was: the toast, then the sound and the hook (each refused by
/// its stand-in, which changes nothing), and no events file at all.
#[test]
fn with_no_lease_announce_is_what_it_was_and_writes_no_line() {
    let a = Announcing::new(ToastDelivery::Herdr);
    let logged = a.announce();
    assert!(logged.contains("display-message"), "{logged}");
    a.wait_for_hook_and_sound(1);
    assert!(
        !a.paths.events_file().exists(),
        "no events file without a lease"
    );
}

/// A toast another machine announced, with a shell substitution and a format in its words, is drawn on a real
/// client as the literal text: the substitution is not run (the marker file is never made) and the format is
/// not expanded (the toast shows `#{pane_id}`, not a pane id).
#[test]
fn a_forwarded_toast_with_a_substitution_in_it_is_drawn_literally_and_runs_nothing() {
    let a = Announcing::new(ToastDelivery::Herdr);
    let marker = a.tmp.path().join("marker");
    let body = format!(
        "#{{pane_id}} #(touch '{}') $(touch '{}')",
        marker.display(),
        marker.display()
    );
    let before = a.s.0.run(&["show-messages"]).unwrap();

    notify::show_forwarded(
        &a.s.0,
        &a.cfg,
        &notify::Forwarded {
            machine: "studio".into(),
            agent: "claude".into(),
            location: "garden:1".into(),
            class: Class::Request,
            title: "claude blocked".into(),
            body: body.clone(),
            dropped: false,
        },
    )
    .unwrap();

    let after = a.s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let logged = after.lines().take(new_count).collect::<Vec<_>>().join("\n");
    assert!(
        logged.contains("display-message"),
        "positive: drawn: {logged}"
    );
    assert!(logged.contains("studio"), "{logged}");
    assert!(
        logged.contains("##{pane_id}"),
        "the format is escaped: {logged}"
    );
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(!marker.exists(), "the substitution ran");
}

/// A toast another machine announced is drawn and nothing more: with the sound on (a request path and a
/// recorder in place of the player) and a hook configured, neither runs and no events line is written. The
/// toast is seen first; then, after a moment, the recorders are still empty; and the same recorders do run
/// for an announce of this machine's own, so an empty log means something.
#[test]
fn a_forwarded_toast_plays_no_sound_runs_no_hook_and_writes_no_events_line() {
    let a = Announcing::new(ToastDelivery::Herdr);
    let before = a.s.0.run(&["show-messages"]).unwrap();

    {
        let _guard = with_sound_cmd(&a.sound_script);
        notify::show_forwarded(
            &a.s.0,
            &a.cfg,
            &notify::Forwarded {
                machine: "studio".into(),
                agent: "claude".into(),
                location: "garden:1".into(),
                class: Class::Request,
                title: "claude blocked".into(),
                body: "needs input".into(),
                dropped: false,
            },
        )
        .unwrap();
    }

    let after = a.s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let logged = after.lines().take(new_count).collect::<Vec<_>>().join("\n");
    assert!(
        logged.contains("display-message") && logged.contains("needs input"),
        "positive: the toast is drawn: {logged}"
    );
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(!a.sound_log.exists(), "the sound played");
    assert!(!a.hook_log.exists(), "the hook ran");
    assert!(
        !a.paths.events_file().exists(),
        "an events line was written"
    );

    // The recorders do work: this machine's own toast, announced the usual way, plays and runs each once.
    a.announce();
    a.wait_for_hook_and_sound(1);
}

/// Under `[ui.toast] delivery = "system"` a toast another machine announced is posted on this Mac's desktop
/// by `show_forwarded` (what the app runs in its own process), with the machine ahead of the title: the
/// notifier's stand-in is given the title and the body, and the tmux clients are drawn nothing.
#[test]
fn a_forwarded_toast_under_system_reaches_the_desktop_notifier_stand_in() {
    let a = Announcing::new(ToastDelivery::System);
    let log = a.tmp.path().join("notifier.log");
    let script = a.tmp.path().join("notifier.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\" >> '{}'\n",
            log.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&script);
    let before = a.s.0.run(&["show-messages"]).unwrap();

    {
        let _guard = with_notify_cmd(&script);
        notify::show_forwarded(
            &a.s.0,
            &a.cfg,
            &notify::Forwarded {
                machine: "studio".into(),
                agent: "claude".into(),
                location: "garden:1".into(),
                class: Class::Request,
                title: "claude blocked".into(),
                body: "needs input".into(),
                dropped: false,
            },
        )
        .unwrap();
    }

    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default(),
        "studio: claude blocked|needs input\n"
    );
    let after = a.s.0.run(&["show-messages"]).unwrap();
    let new_count = after.lines().count().saturating_sub(before.lines().count());
    let logged = after.lines().take(new_count).collect::<Vec<_>>().join("\n");
    assert!(!logged.contains("needs input"), "nothing drawn: {logged}");
}
