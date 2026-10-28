//! Every lifecycle event, end to end: the real plugin script's hooks and the real `taw`
//! verbs, against a throwaway tmux server, with `tests/fixtures/plugins/echo-plugin`
//! linked so each event leaves `event-<name>.json` behind.
//!
//! `tests/bats/plugin.bats` proves the hook *strings* are installed once each; what a
//! fake `taw` cannot show is that the hook fires, that the binary accepts the arguments
//! the hook was built with, and that the payload carries the ids herdr's own event data
//! carries. That is what this file is for.
//!
//! The single-source rule is what makes it worth testing this way: exactly one of a tmux
//! hook or a `taw` verb emits each event (see `src/cli/events.rs`), so every assertion
//! here is also a guard against the same change firing an event twice.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use common::shell_env::ShellHome;
use common::{Server, ServerOptions, taw_bin, wait_for, write_script};

/// Ceiling for a hook-driven side effect: hooks run with `run-shell -b` and the plugin's
/// own event command is spawned detached on top of that, so two asynchronous hops.
const HOOK: Duration = Duration::from_secs(5);

/// Ceiling for a whole source of the plugin script to get as far as a test wants it, through the
/// `tmux` stand-in that stops it there. The script makes some hundred tmux calls, each of them one more
/// shell script when a stand-in is first on its `PATH`, so a source takes seconds on a quiet machine
/// and a good deal more on a loaded one: the test of an exit during a source waited [`HOOK`] for the
/// source to reach its stop and failed at a load of 65 to 78 from other sessions, though it passes in
/// about seven seconds alone. A wait on a condition costs a run that passes nothing, so the ceiling is
/// sized for the slow machine and not for the usual one.
const SOURCE: Duration = Duration::from_secs(30);

/// How much longer to watch after an event has arrived, to catch a *second* delivery of
/// the same event. Comfortably more than the two asynchronous hops one delivery takes,
/// and well under [`HOOK`].
const SETTLE: Duration = Duration::from_millis(1500);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

fn echo_plugin() -> PathBuf {
    common::repo_root()
        .join("tests")
        .join("fixtures")
        .join("plugins")
        .join("echo-plugin")
}

/// A server with the plugin script sourced and the echo plugin linked.
fn server() -> Server {
    let server = Server::start();
    server.write_config(CONFIG);
    server.ok(
        &["plugin", "link", echo_plugin().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();
    server
}

/// `<state>/plugins/state/echo-plugin/event-<event>.json`.
fn event_file(server: &Server, event: &str) -> PathBuf {
    server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("echo-plugin")
        .join(format!("event-{event}.json"))
}

/// Waits for the plugin's hook to have recorded `event`, then parses the context JSON it
/// was handed. Panics with the state dir's contents when it never arrives — the failure
/// is nearly always "a different event fired", and the listing says which.
fn recorded(server: &Server, event: &str) -> Value {
    let path = event_file(server, event);
    let ready = wait_for(
        || {
            std::fs::metadata(&path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        },
        HOOK,
    );
    assert!(
        ready,
        "'{event}' never reached the plugin; recorded events: {:?}",
        recorded_events(server)
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("'{event}' context is not JSON ({e}): {raw}"))
}

fn recorded_events(server: &Server) -> Vec<String> {
    let dir = event_file(server, "x").parent().unwrap().to_path_buf();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Asserts `event` has *not* been recorded. Used for the double-emit guards, where the
/// point is that a change fired one event and not another.
fn not_recorded(server: &Server, event: &str) {
    assert!(
        !event_file(server, event).exists(),
        "'{event}' fired but nothing should have emitted it; recorded: {:?}",
        recorded_events(server)
    );
}

/// Waits until the pane roster lists `public`.
///
/// The barrier every close assertion needs. The roster is refreshed by a hook command
/// that tmux runs **detached**, so a pane created and killed inside that latency is
/// simply not in the set the diff compares against, and its close goes unreported. A real
/// pane outlives the latency by orders of magnitude; a test that splits and kills in
/// consecutive statements does not, so it waits for the product to have caught up rather
/// than racing it.
fn roster_lists(server: &Server, public: &str) {
    assert!(
        wait_for(
            || server
                .global_opt("@agent_wizard_panes")
                .split_whitespace()
                .any(|id| id == public),
            HOOK
        ),
        "the roster never listed {public}; it holds {:?}",
        server.global_opt("@agent_wizard_panes")
    );
}

/// Removes a recorded event so a later step can assert it fired *again*.
fn forget(server: &Server, event: &str) {
    let _ = std::fs::remove_file(event_file(server, event));
}

/// `<state>/plugins/state/echo-plugin/tally-<event>.log` — one line per delivery, which
/// the fixture appends for the three "created" events on top of the context it records.
fn tally_file(server: &Server, event: &str) -> PathBuf {
    server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("echo-plugin")
        .join(format!("tally-{event}.log"))
}

fn tally_lines(path: &PathBuf) -> usize {
    std::fs::read_to_string(path)
        .map(|raw| raw.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// How many times `event` has reached the plugin, counted once it has arrived *and* the
/// window in which a duplicate would have landed has passed.
///
/// This is the double-emit guard. Each of the three "created" events is fired by a tmux
/// hook that the `taw` verb for the same change also trips, so "the verb emits nothing
/// itself" is the only thing keeping these at one — and a plain `>= 1` assertion would
/// not notice it being undone.
fn deliveries(server: &Server, event: &str) -> usize {
    let path = tally_file(server, event);
    assert!(
        wait_for(|| tally_lines(&path) >= 1, HOOK),
        "'{event}' never reached the plugin; recorded events: {:?}",
        recorded_events(server)
    );
    std::thread::sleep(SETTLE);
    tally_lines(&path)
}

/// Asserts a hook-built record and the verb's own reply describe the same thing with the
/// same shape: identical field sets, and identical values for every field that is not a
/// live reading.
///
/// `agent_status`, `focused` and the terminal title are deliberately left out. The verb
/// reads tmux as it returns and the hook's `taw` reads it again a moment later, and in
/// between the pane watcher the same hooks attach can legitimately have written an agent
/// state — a drift that says nothing about whether the payload changed.
fn same_record(event: &Value, reply: &Value, what: &str) {
    let fields = |value: &Value| -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .unwrap_or_else(|| panic!("{what} is not an object: {value}"))
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        fields(event),
        fields(reply),
        "{what}: the hook's payload and the verb's reply carry different fields"
    );
    for field in [
        "pane_id",
        "tab_id",
        "workspace_id",
        "terminal_id",
        "active_tab_id",
        "label",
        "number",
        "pane_count",
        "tab_count",
        "cwd",
    ] {
        if let Some(expected) = reply.get(field) {
            assert_eq!(
                event.get(field),
                Some(expected),
                "{what}.{field}: hook payload {event} vs verb reply {reply}"
            );
        }
    }
}

#[test]
fn the_tab_hooks_emit_closed_focused_and_renamed_with_herdrs_ids() {
    let server = server();
    let workspace = server.ok(
        &["workspace", "list"],
        "cli:workspace:list",
        "workspace_list",
    )["workspaces"][0]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // --- tab.renamed, from `after-rename-window` ---
    let created = server.ok(
        &["tab", "create", "--label", "victim"],
        "cli:tab:create",
        "tab_created",
    );
    let tab_id = created["tab"]["tab_id"].as_str().unwrap().to_string();
    // `tab create` is verb-sourced, and its own hook (`window-linked`) is deliberately not
    // installed — so exactly one `tab.created` arrives, from the verb.
    let created_event = recorded(&server, "tab.created");
    assert_eq!(created_event["tab"]["tab_id"], tab_id.as_str());

    server.ok(
        &["tab", "rename", &tab_id, "renamed", "with", "spaces"],
        "cli:tab:rename",
        "tab",
    );
    let renamed = recorded(&server, "tab.renamed");
    assert_eq!(renamed["tab_id"], tab_id.as_str());
    assert_eq!(renamed["workspace_id"], workspace.as_str());
    // A label with spaces survives `#{q:window_name}` intact.
    assert_eq!(renamed["label"], "renamed with spaces");
    // The bare ids still populate the record slots, which is where HERDR_TAB_ID comes from.
    assert_eq!(renamed["tab"]["tab_id"], tab_id.as_str());
    assert_eq!(renamed["workspace"]["workspace_id"], workspace.as_str());

    // --- tab.focused, from `session-window-changed` ---
    server.ok(&["tab", "focus", &tab_id], "cli:tab:focus", "ok");
    let focused = recorded(&server, "tab.focused");
    assert_eq!(focused["tab_id"], tab_id.as_str());
    assert_eq!(focused["workspace_id"], workspace.as_str());

    // --- tab.closed, from `window-unlinked` ---
    server.ok(&["tab", "close", &tab_id], "cli:tab:close", "ok");
    let closed = recorded(&server, "tab.closed");
    assert_eq!(closed["tab_id"], tab_id.as_str());
    // The window is gone by the time the hook runs, so this id can only have been
    // computed from the raw ids the hook passed — never looked up.
    assert_eq!(closed["workspace_id"], workspace.as_str());
}

#[test]
fn a_creation_made_with_plain_tmux_fires_exactly_one_created_event() {
    // The gap these three hooks close: `prefix c`, `prefix %` and `tmux new-session` never
    // pass through `taw`, so while the verbs owned the "created" events a window, pane or
    // session the user made with tmux itself was invisible to every plugin subscribed to
    // them. Each assertion is made *before* the next step, because one creation can
    // legitimately fire another's event (a new session brings a new window with it).
    let server = server();
    let session = server.tmux_ok(&["display-message", "-p", "#{session_id}"]);
    let workspace = format!("w{}", session.trim_start_matches('$'));

    // --- tab.created, from `window-linked` ---
    let window = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-n",
        "byhand",
    ]);
    let expected_tab = format!("{workspace}:t{}", window.trim_start_matches('@'));
    let created = recorded(&server, "tab.created");
    assert_eq!(created["tab"]["tab_id"], expected_tab.as_str());
    assert_eq!(created["tab"]["workspace_id"], workspace.as_str());
    assert_eq!(created["tab"]["label"], "byhand");
    // The tab's root pane rides along as context, exactly as `taw tab create` sent it.
    assert_eq!(created["pane"]["tab_id"], expected_tab.as_str());
    assert!(
        created["pane"]["terminal_id"].as_str().is_some(),
        "{created}"
    );
    assert_eq!(deliveries(&server, "tab.created"), 1);

    // --- pane.created, from `after-split-window` ---
    let pane = server.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &window,
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let created = recorded(&server, "pane.created");
    assert_eq!(created["pane"]["terminal_id"], pane.as_str());
    assert_eq!(created["pane"]["workspace_id"], workspace.as_str());
    assert_eq!(deliveries(&server, "pane.created"), 1);

    // --- workspace.created, from `session-created` ---
    let new_session = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "byhand-ws",
        "-P",
        "-F",
        "#{session_id}",
    ]);
    let expected_workspace = format!("w{}", new_session.trim_start_matches('$'));
    let created = recorded(&server, "workspace.created");
    assert_eq!(
        created["workspace"]["workspace_id"],
        expected_workspace.as_str()
    );
    assert_eq!(created["workspace"]["label"], "byhand-ws");
    // The session's first tab and root pane come with it, as `taw workspace create` sent
    // them — a plugin sees the whole new workspace from the one event.
    assert_eq!(created["tab"]["workspace_id"], expected_workspace.as_str());
    assert_eq!(created["pane"]["workspace_id"], expected_workspace.as_str());
    assert_eq!(deliveries(&server, "workspace.created"), 1);
}

#[test]
fn the_creation_verbs_still_deliver_one_created_event_each_and_the_same_payload() {
    // The other half of moving these three onto the hooks: a verb runs the very tmux
    // command whose hook now fires, so it must emit nothing itself. What a plugin
    // receives has to be unchanged — same event, once, carrying the same records the
    // verb's own reply does.
    let server = server();

    // --- `taw tab create` -> `new-window` -> `window-linked` ---
    let reply = server.ok(
        &["tab", "create", "--label", "verb-tab", "--no-focus"],
        "cli:tab:create",
        "tab_created",
    );
    let event = recorded(&server, "tab.created");
    same_record(&event["tab"], &reply["tab"], "tab.created.tab");
    same_record(&event["pane"], &reply["root_pane"], "tab.created.pane");
    assert_eq!(deliveries(&server, "tab.created"), 1);

    // --- `taw pane split` -> `split-window` -> `after-split-window` ---
    let reply = server.ok(
        &[
            "pane",
            "split",
            &server.public_pane_id(&server.pane()),
            "--no-focus",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let event = recorded(&server, "pane.created");
    same_record(&event["pane"], &reply["pane"], "pane.created.pane");
    assert_eq!(deliveries(&server, "pane.created"), 1);

    // --- `taw workspace create` -> `new-session` -> `session-created` ---
    let reply = server.ok(
        &["workspace", "create", "--label", "verb-ws", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    let event = recorded(&server, "workspace.created");
    same_record(
        &event["workspace"],
        &reply["workspace"],
        "workspace.created.workspace",
    );
    same_record(&event["tab"], &reply["tab"], "workspace.created.tab");
    same_record(
        &event["pane"],
        &reply["root_pane"],
        "workspace.created.pane",
    );
    assert_eq!(deliveries(&server, "workspace.created"), 1);
    // The new session brought a window with it, so its `window-linked` fired too: one
    // `tab.created` for the tab the workspace came with, on top of the verb-tab above.
    // That is herdr's own cascade (`emit_workspace_open_events`), not a double-emit.
    assert_eq!(deliveries(&server, "tab.created"), 2);
}

/// A `tmux` that renames the window of the session `taw workspace create` makes after the verb has read
/// it and before its `session-created` hook does, as tmux's automatic-rename does on a loaded machine
/// (the window is named `sh` when it is made, and `bash` once its shell runs). The hook's read of the
/// windows waits until the verb has begun to look for the event's payload, or has ended (all its own
/// reads done), or for five seconds, and renames the window first. It notes in `verb-read-the-windows`
/// that the verb read the windows itself. A process is told by what its parent (`taw`) was started as.
fn tmux_that_renames_between_the_reads(dir: &Path) -> PathBuf {
    let wrapper = dir.join("tmux-that-renames-between-the-reads");
    let read_them = dir.join("verb-read-the-windows");
    let (looking, verb) = (dir.join("verb-looks-for-the-payload"), dir.join("verb.pid"));
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    write_script(
        &wrapper,
        &format!(
            "caller=$(ps -o command= -p $PPID)\n\
             case \"$caller\" in\n\
             \x20 *'plugin event workspace.created'*)\n\
             \x20   case \"$*\" in\n\
             \x20     *list-windows*)\n\
             \x20       i=0\n\
             \x20       while [ ! -e '{looking}' ] && {{ [ ! -s '{verb}' ] || kill -0 \"$(cat '{verb}')\" 2>/dev/null; }} && [ \"$i\" -lt 100 ]; do\n\
             \x20         sleep 0.05; i=$((i + 1))\n\
             \x20       done\n\
             \x20       session=$(printf '%s\\n' \"$caller\" | sed -n 's/.*--workspace \\([^ ]*\\).*/\\1/p')\n\
             \x20       '{real}' -L \"$TAW_TMUX_SOCKET_NAME\" rename-window -t \"$session:\" renamed-between-reads\n\
             \x20       ;;\n\
             \x20   esac\n\
             \x20   ;;\n\
             \x20 *'workspace create'*)\n\
             \x20   printf '%s\\n' \"$PPID\" > '{verb}'\n\
             \x20   case \"$*\" in\n\
             \x20     *agent_wizard_created*) : > '{looking}' ;;\n\
             \x20     *list-windows*) : > '{read_them}' ;;\n\
             \x20   esac\n\
             \x20   ;;\n\
             esac\n\
             exec '{real}' \"$@\"\n",
            looking = looking.display(),
            read_them = read_them.display(),
            verb = verb.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// What `taw workspace create` answers and what the `workspace.created` event carries are one read of
/// the new workspace. Read twice, they could differ: tmux's automatic-rename renamed the new window
/// between the two reads under load, and the plugin was told one name and the caller another. A `tmux`
/// that renames the window between the verb's read and the hook's makes it happen every time.
#[test]
fn a_rename_between_the_reads_changes_nothing_between_the_created_event_and_the_reply() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmux_that_renames_between_the_reads(tmp.path());
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        env: vec![
            ("TAW_TMUX_BIN".to_string(), wrapper.display().to_string()),
            ("HOME".to_string(), home.display().to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server.ok(
        &["plugin", "link", echo_plugin().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();

    let reply = server.ok(
        &["workspace", "create", "--label", "verb-ws", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );

    let event = recorded(&server, "workspace.created");
    // The rename did come between the two reads: the event carries the name it gave the window.
    assert_eq!(event["tab"]["label"], "renamed-between-reads", "{event}");
    same_record(
        &event["workspace"],
        &reply["workspace"],
        "workspace.created.workspace",
    );
    same_record(&event["tab"], &reply["tab"], "workspace.created.tab");
    same_record(
        &event["pane"],
        &reply["root_pane"],
        "workspace.created.pane",
    );
    // The verb answered with what the event carried: it read none of the windows itself.
    assert!(
        !tmp.path().join("verb-read-the-windows").exists(),
        "the verb read the windows itself instead of answering with the event's payload"
    );
    // The payload the hook left on the session for the verb is taken off once read.
    let session = reply["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .replacen('w', "$", 1);
    assert_eq!(
        server.tmux_ok(&[
            "show-options",
            "-qv",
            "-t",
            &session,
            "@agent_wizard_created"
        ]),
        "",
        "the payload stayed on the session"
    );
}

/// A real client attached to `server`'s session `one`: a `tmux` client in a pane of a second tmux server,
/// which is killed with this value, so that a `switch-client` has a client to move. The second server's
/// shells have a home of their own and no history file (`common::shell_env`), which go once it has.
struct AttachedClient {
    outer: String,
    _home: ShellHome,
}

impl AttachedClient {
    fn attach(server: &Server) -> AttachedClient {
        let outer = format!("{}-outer", server.socket());
        let attach = format!(
            "env -u TMUX PATH=/usr/bin:/bin '{}' -L '{}' attach-session -t one",
            taw::tmux::Tmux::with_socket_name("unused").bin.display(),
            server.socket()
        );
        let home = ShellHome::new();
        home.start_server(
            &taw::tmux::Tmux::with_socket_name(&outer),
            &["-d", "-s", "host", "-x", "100", "-y", "30", &attach],
        );
        let client = AttachedClient { outer, _home: home };
        assert!(
            wait_for(
                || !server
                    .tmux_ok(&["list-clients", "-F", "#{client_tty}"])
                    .is_empty(),
                HOOK
            ),
            "no client attached to the server"
        );
        client
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.outer, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .status();
    }
}

/// A `tmux` for `taw workspace create` that holds its `switch-client` until the new session's
/// `session-created` hook has left the event's payload on it (`@agent_wizard_created`), so that the hook
/// reads the workspace before a client is moved onto it, and notes in `held` that it did.
fn tmux_that_switches_after_the_hook_read(dir: &Path) -> PathBuf {
    let wrapper = dir.join("tmux-that-switches-late");
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             if [ \"$4\" = switch-client ]; then\n\
             \x20 i=0\n\
             \x20 while [ \"$i\" -lt 100 ]; do\n\
             \x20   if [ -n \"$(\"$real\" -L \"$2\" show-options -qv -t \"$6\" @agent_wizard_created 2>/dev/null)\" ]; then\n\
             \x20     : > '{held}'; break\n\
             \x20   fi\n\
             \x20   sleep 0.05; i=$((i + 1))\n\
             \x20 done\n\
             fi\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            held = dir.join("held").display(),
        ),
    );
    wrapper
}

/// `taw workspace create` moves a client onto the workspace it makes, and answers with the payload its
/// `workspace.created` event carries, which the hook builds from a read of the workspace that can come
/// before the move: that read says no client is on it. What the verb answers says whether a client is on
/// the workspace once it has moved one there, read again after the move.
#[test]
fn workspace_create_answers_focused_for_the_workspace_it_moved_a_client_onto() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        env: vec![("HOME".to_string(), home.display().to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server.ok(
        &["plugin", "link", echo_plugin().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();
    let _client = AttachedClient::attach(&server);
    let late = tmux_that_switches_after_the_hook_read(tmp.path());

    let (code, envelope) = server.taw_with_env(
        &["workspace", "create", "--label", "moved-to"],
        &[("TAW_TMUX_BIN", late.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{envelope}");
    assert!(
        tmp.path().join("held").exists(),
        "the switch was not held until the hook had read the workspace"
    );
    assert_eq!(
        server.tmux_ok(&["list-clients", "-F", "#{client_session}"]),
        "moved-to",
        "the client was not moved onto the new workspace"
    );
    let reply = &envelope["result"];
    assert_eq!(
        reply["workspace"]["focused"],
        Value::Bool(true),
        "the reply says no client is on the workspace it moved one onto: {reply}"
    );
    // The event says what the hook read, before the move.
    assert_eq!(
        recorded(&server, "workspace.created")["workspace"]["focused"],
        Value::Bool(false)
    );
}

/// A `tmux` that writes each command it is given to `log`, one line each, and runs it.
fn tmux_that_logs_its_commands(dir: &Path) -> (PathBuf, PathBuf) {
    let (wrapper, log) = (dir.join("tmux-that-logs"), dir.join("tmux-commands.log"));
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    write_script(
        &wrapper,
        &format!(
            "{{ printf '%s' \"$*\" | tr '\\n\\t\\r' '   '; echo; }} >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    );
    (wrapper, log)
}

/// A creation verb waits for the event's payload only when the event is built: with no plugin
/// listening for it, or with taw's hooks not on the server, it reads the new object itself at once and
/// never looks for a payload.
#[test]
fn a_creation_verb_waits_for_no_payload_when_no_event_is_built() {
    for (listening, hooked) in [(false, true), (true, false)] {
        let tmp = tempfile::tempdir().unwrap();
        let (wrapper, log) = tmux_that_logs_its_commands(tmp.path());
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let server = Server::start_with(ServerOptions {
            env: vec![
                ("TAW_TMUX_BIN".to_string(), wrapper.display().to_string()),
                ("HOME".to_string(), home.display().to_string()),
            ],
            ..ServerOptions::default()
        });
        server.write_config(CONFIG);
        if listening {
            server.ok(
                &["plugin", "link", echo_plugin().to_str().unwrap()],
                "cli:plugin:link",
                "plugin_linked",
            );
        }
        if hooked {
            server.source_plugin();
        }

        let reply = server.ok(
            &["tab", "create", "--label", "quiet", "--no-focus"],
            "cli:tab:create",
            "tab_created",
        );

        assert_eq!(reply["tab"]["label"], "quiet", "{reply}");
        let commands = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !commands.contains("@agent_wizard_created"),
            "listening {listening}, hooked {hooked}: a payload was looked for:\n{commands}"
        );
    }
}

#[test]
fn a_tab_created_with_a_tmux_key_still_fires_the_focus_and_close_events() {
    // The whole reason the hooks exist: `prefix c` / `prefix &` never pass through `taw`.
    let server = server();
    let window = server.tmux_ok(&["new-window", "-P", "-F", "#{window_id}", "-n", "byhand"]);
    let session = server.tmux_ok(&["display-message", "-p", "#{session_id}"]);
    let expected_tab = format!(
        "w{}:t{}",
        session.trim_start_matches('$'),
        window.trim_start_matches('@')
    );

    // `new-window` without `-d` selects it, which is the session's window change.
    let focused = recorded(&server, "tab.focused");
    assert_eq!(focused["tab_id"], expected_tab.as_str());

    let victim_pane = server.tmux_ok(&["display-message", "-p", "-t", &window, "#{pane_id}"]);
    roster_lists(
        &server,
        &format!(
            "w{}:p{}",
            session.trim_start_matches('$'),
            victim_pane.trim_start_matches('%')
        ),
    );
    server.tmux_ok(&["kill-window", "-t", &window]);
    let closed = recorded(&server, "tab.closed");
    assert_eq!(closed["tab_id"], expected_tab.as_str());
    // The window took its pane with it, and the roster diff on `window-unlinked` notices.
    // No verb could have: none ran.
    let pane_closed = recorded(&server, "pane.closed");
    assert_eq!(
        pane_closed["workspace_id"],
        format!("w{}", session.trim_start_matches('$')).as_str()
    );
}

#[test]
fn the_workspace_hooks_emit_renamed_and_closed_and_report_metadata_emits_updated() {
    let server = server();
    let created = server.ok(
        &["workspace", "create", "--label", "second"],
        "cli:workspace:create",
        "workspace_created",
    );
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // --- workspace.renamed, from `after-rename-session` ---
    server.ok(
        &["workspace", "rename", &workspace, "renamed-ws"],
        "cli:workspace:rename",
        "workspace",
    );
    let renamed = recorded(&server, "workspace.renamed");
    assert_eq!(renamed["workspace_id"], workspace.as_str());
    assert_eq!(renamed["label"], "renamed-ws");

    // --- workspace.updated, from the verb (tmux fires nothing for an option write) ---
    server.ok(
        &[
            "workspace",
            "report-metadata",
            &workspace,
            "--source",
            "test",
            "--token",
            "ticket=ABC-1",
        ],
        "cli:workspace:report-metadata",
        "ok",
    );
    let updated = recorded(&server, "workspace.updated");
    assert_eq!(updated["workspace"]["workspace_id"], workspace.as_str());

    // --- workspace.closed, from `session-closed` ---
    server.ok(
        &["workspace", "close", &workspace],
        "cli:workspace:close",
        "ok",
    );
    let closed = recorded(&server, "workspace.closed");
    // The session is destroyed before the hook runs: this id is arithmetic on the raw
    // `$N` the hook passed, which is the only thing that still exists.
    assert_eq!(closed["workspace_id"], workspace.as_str());
}

#[test]
fn the_pane_events_come_from_the_hook_or_the_verb_that_owns_each_one() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);

    // --- layout.updated, from `window-layout-changed`; pane.created from the verb ---
    let split = server.ok(
        &["pane", "split", &public, "--direction", "right"],
        "cli:pane:split",
        "pane_split",
    );
    let new_pane = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let layout = recorded(&server, "layout.updated");
    assert!(layout["layout"]["tab_id"].as_str().is_some(), "{layout}");
    let panes = layout["layout"]["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 2, "layout: {}", layout["layout"]);
    assert_eq!(layout["layout"]["splits"][0]["id"], "root");
    assert_eq!(layout["layout"]["splits"][0]["direction"], "right");

    // --- pane.focused, from `after-select-pane` ---
    forget(&server, "layout.updated");
    server.ok(
        &["pane", "focus", &new_pane, "--direction", "left"],
        "cli:pane:focus",
        "pane",
    );
    let focused = recorded(&server, "pane.focused");
    assert_eq!(focused["pane_id"], public.as_str());
    assert_eq!(focused["pane"]["pane_id"], public.as_str());
    // Focusing moves nothing, so no layout change may be reported for it.
    not_recorded(&server, "layout.updated");

    // --- pane.updated, from `pane rename` ---
    server.ok(
        &["pane", "rename", &new_pane, "build", "log"],
        "cli:pane:rename",
        "pane",
    );
    let updated = recorded(&server, "pane.updated");
    assert_eq!(updated["pane"]["pane_id"], new_pane.as_str());
    assert_eq!(updated["pane"]["title"], "build log");

    // --- pane.closed, from the `after-kill-pane` roster diff, which `pane close` trips ---
    roster_lists(&server, &new_pane);
    server.ok(&["pane", "close", &new_pane], "cli:pane:close", "ok");
    let closed = recorded(&server, "pane.closed");
    assert_eq!(closed["pane_id"], new_pane.as_str());
    // …and the same kill *is* a layout change, which the hook reports.
    let layout = recorded(&server, "layout.updated");
    assert_eq!(layout["layout"]["panes"].as_array().unwrap().len(), 1);
}

#[test]
fn a_shell_that_exits_fires_pane_exited_with_the_panes_own_workspace() {
    let server = server();
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let new_pane = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let terminal = split["pane"]["terminal_id"].as_str().unwrap().to_string();

    // The pane's own shell exiting is the one pane event no `taw` verb causes. It exits as soon after
    // the split as it can, while the split's own `pane.closed` detection may still be running: what
    // that writes must not put the pane back (the test below holds it back to make sure it is so).
    server.tmux_ok(&["send-keys", "-t", &terminal, "exit", "Enter"]);
    let exited = recorded(&server, "pane.exited");
    assert_eq!(exited["pane_id"], new_pane.as_str());
    assert_eq!(
        exited["workspace_id"],
        server.ok(
            &["workspace", "list"],
            "cli:workspace:list",
            "workspace_list"
        )["workspaces"][0]["workspace_id"]
    );
    // A process ending is **not** a close. herdr emits `PaneExited` from
    // `AppEvent::PaneDied` and `PaneClosed` only from its explicit close path, and its own
    // `pane_exit_emits_layout_updated_when_tab_survives` pins that a dead pane emits
    // `pane.exited` then `layout.updated` and nothing else. The exited pane is dropped
    // from the roster silently, so no later detection calls it a close either.
    not_recorded(&server, "pane.closed");
    server.ok(
        &["pane", "split", &server.public_pane_id(&server.pane())],
        "cli:pane:split",
        "pane_split",
    );
    assert!(
        !common::wait_for(|| event_file(&server, "pane.closed").exists(), HOOK),
        "a later detection reported the exited pane as closed"
    );
}

/// A `tmux` that makes the first write of the pane roster wait, for ten seconds at most, until
/// the second has landed, and runs the real one: the order a slow `pane.closed` detection leaves its
/// write in when the pane exits inside the time it takes, with the exit handled in between. It holds
/// back whatever call sets `@agent_wizard_panes` first, however the roster is written, so it says the
/// same of any way of writing it. It says in `order/gave-up` when it ran out of time, so that a test
/// cannot pass for a hold that never held.
fn tmux_that_holds_the_first_roster_write_back(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let order = dir.join("order");
    std::fs::create_dir_all(&order).unwrap();
    let wrapper = dir.join("tmux-holding-the-first-roster-write");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *\"set-option -g @agent_wizard_panes\"*)\n    if mkdir '{o}/first' 2>/dev/null; then\n      i=0\n      while [ ! -e '{o}/second-done' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n      [ -e '{o}/second-done' ] || : > '{o}/gave-up'\n    elif mkdir '{o}/second' 2>/dev/null; then\n      '{real}' \"$@\"; status=$?; : > '{o}/second-done'; exit $status\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            o = order.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// The detection that the split fires reads the live panes some time before it writes the roster, and
/// the exit of the new pane is handled in that time: the exit handler refreshes the roster, without
/// the pane, and the detection then writes what it read, which lists the pane once more. The next
/// detection then reports a pane that exited as closed, and a user's `pane.closed` hook runs for a
/// pane that has already fired `pane.exited`. A roster write must never put back a pane an exit took
/// out, so the exit here does not wait for the roster to list the pane, and a `tmux` that holds the
/// detection's write back until the exit handler's has landed makes the detection as slow as it can be.
#[test]
fn a_slow_pane_closed_detection_cannot_put_an_exited_pane_back_in_the_roster() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmux_that_holds_the_first_roster_write_back(tmp.path());
    let server = Server::start_with(ServerOptions {
        env: vec![("TAW_TMUX_BIN".to_string(), wrapper.display().to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server.ok(
        &["plugin", "link", echo_plugin().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let new_pane = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let terminal = split["pane"]["terminal_id"].as_str().unwrap().to_string();

    // No wait for the roster to list the pane: the exit is as soon after the split as it can be.
    server.tmux_ok(&["send-keys", "-t", &terminal, "exit", "Enter"]);
    let exited = recorded(&server, "pane.exited");
    assert_eq!(exited["pane_id"], new_pane.as_str());
    assert!(
        !tmp.path().join("order").join("gave-up").exists(),
        "the first write was let go by the clock before the second landed; this run proves nothing"
    );
    not_recorded(&server, "pane.closed");
    server.ok(
        &["pane", "split", &server.public_pane_id(&server.pane())],
        "cli:pane:split",
        "pane_split",
    );
    assert!(
        !wait_for(|| event_file(&server, "pane.closed").exists(), HOOK),
        "a later detection reported the exited pane as closed (the roster holds {:?})",
        server.global_opt("@agent_wizard_panes")
    );
}

/// A `tmux` that, once `armed` exists, makes the first `pane.closed` detection wait before its second
/// read of what it writes the roster from (the roster and the live panes: whichever it reads second),
/// until `release` exists, and says so in `held`. It says in `wrote` when that detection has written
/// the roster. The detection is told apart from the other processes by the command line of the one
/// that runs the `tmux`. It is held for ten seconds at most, and says in `gave-up` when it ran out of
/// time, so that a test cannot pass for a hold that never held.
fn tmux_that_holds_a_detection_between_its_reads(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-a-detection-between-its-reads");
    write_script(
        &wrapper,
        &format!(
            "o='{o}'\ncase \"$*\" in\n  *\"set-option -g @agent_wizard_panes\"*)\n    if [ -e \"$o/reads-$PPID\" ]; then\n      '{real}' \"$@\"; status=$?; : > \"$o/wrote\"; exit $status\n    fi ;;\n  *\" show-options -g\"|*\" list-panes -a -F \"*)\n    if [ -e \"$o/armed\" ]; then\n      case \"$(ps -o command= -p $PPID)\" in\n        *\"plugin event pane.closed\"*)\n          n=$(cat \"$o/reads-$PPID\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$o/reads-$PPID\"\n          if [ $n -eq 2 ] && mkdir \"$o/held\" 2>/dev/null; then\n            i=0\n            while [ ! -e \"$o/release\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n            [ -e \"$o/release\" ] || : > \"$o/gave-up\"\n          fi ;;\n      esac\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            o = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// A detection reads the roster and the live panes and writes the one over the other, so which it
/// reads first says whether an exit handled between the two reads can be written over. Read the
/// roster (with its generation) first and the panes after it, and a write that landed between the two
/// reads changes the generation the write is checked against; read the panes first, and the roster
/// after an exit has refreshed it, and the write is checked against a generation that already has the
/// exit in it while it carries the panes from before: it lands, and puts the pane back. A `tmux`
/// that holds the detection between its two reads while the pane exits shows which it does.
#[test]
fn an_exit_between_the_two_reads_of_a_detection_does_not_put_the_pane_back() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmux_that_holds_a_detection_between_its_reads(tmp.path());
    let server = Server::start_with(ServerOptions {
        env: vec![("TAW_TMUX_BIN".to_string(), wrapper.display().to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server.ok(
        &["plugin", "link", echo_plugin().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let victim = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let terminal = split["pane"]["terminal_id"].as_str().unwrap().to_string();
    // The roster knows the pane that is about to exit, and the detection of this split is done.
    roster_lists(&server, &victim);

    // A second split fires a detection that finds the pane alive, and holds it between its reads.
    std::fs::write(tmp.path().join("armed"), "").unwrap();
    server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    assert!(
        wait_for(|| tmp.path().join("held").exists(), HOOK),
        "the detection never reached its second read"
    );

    // The pane exits while the detection is held, and the exit refreshes the roster without it.
    server.tmux_ok(&["send-keys", "-t", &terminal, "exit", "Enter"]);
    let exited = recorded(&server, "pane.exited");
    assert_eq!(exited["pane_id"], victim.as_str());
    assert!(
        wait_for(
            || !server
                .global_opt("@agent_wizard_panes")
                .split_whitespace()
                .any(|id| id == victim),
            HOOK
        ),
        "the exit never took the pane out of the roster"
    );

    std::fs::write(tmp.path().join("release"), "").unwrap();
    assert!(
        wait_for(|| tmp.path().join("wrote").exists(), HOOK),
        "the held detection never wrote the roster"
    );
    assert!(
        !tmp.path().join("gave-up").exists(),
        "the detection was let go by the clock before the exit was handled; this run proves nothing"
    );
    let roster = server.global_opt("@agent_wizard_panes");
    assert!(
        !roster.split_whitespace().any(|id| id == victim),
        "the exited pane {victim} is back in the roster: {roster:?}"
    );
    not_recorded(&server, "pane.closed");
    server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    assert!(
        !wait_for(|| event_file(&server, "pane.closed").exists(), SETTLE),
        "a later detection reported the exited pane as closed"
    );
}

/// A plugin that keeps *every* `pane.closed` delivery, one context per line.
const PANE_CLOSE_RECORDER: &str = r#"id = "pane-close-recorder"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Test plugin: keeps every pane.closed delivery."

[[events]]
on = "pane.closed"
command = ["sh", "-c", "printf '%s\n' \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$HERDR_PLUGIN_STATE_DIR/closed.log\""]
"#;

/// A `tmux` that, once `arm` exists, makes the first write of the pane roster wait (ten seconds at
/// most) until another has arrived, and runs the real one: both writers have read what they write
/// from before either has written, which is two detections that overlap as much as they can. It says
/// in `gave-up` when it ran out of time, so that a test cannot pass for a meeting that never was.
fn tmux_that_lets_two_roster_writes_meet(dir: &Path, arm: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-meeting-of-roster-writes");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *\"set-option -g @agent_wizard_panes\"*)\n    if [ -e '{arm}' ]; then\n      if mkdir '{dir}/held' 2>/dev/null; then\n        i=0\n        while [ ! -e '{dir}/arrived' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n        [ -e '{dir}/arrived' ] || : > '{dir}/gave-up'\n      else\n        : > '{dir}/arrived'\n      fi\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            arm = arm.display(),
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// A close that two hooks run a detection for (`after-kill-pane`, and `after-split-window` for the
/// split made with it) is one `pane.closed`, though both detections have read the roster from before
/// the close when either writes it. The detection whose write takes the pane out of the roster
/// reports it, and the other, whose write lost, reads the roster again and finds nothing to report.
/// A `tmux` that makes the two meet before either writes shows the pane reported twice by detections
/// that wrote the roster one over the other.
#[test]
fn two_detections_that_both_saw_a_pane_close_report_it_once() {
    let tmp = tempfile::tempdir().unwrap();
    let arm = tmp.path().join("arm");
    let wrapper = tmux_that_lets_two_roster_writes_meet(tmp.path(), &arm);
    let server = Server::start_with(ServerOptions {
        env: vec![("TAW_TMUX_BIN".to_string(), wrapper.display().to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let recorder = tempfile::tempdir().unwrap();
    std::fs::write(
        recorder.path().join("herdr-plugin.toml"),
        PANE_CLOSE_RECORDER,
    )
    .unwrap();
    server.ok(
        &["plugin", "link", recorder.path().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let victim = split["pane"]["terminal_id"].as_str().unwrap().to_string();
    let victim_public = split["pane"]["pane_id"].as_str().unwrap().to_string();
    // The roster knows the pane, and the hooks of the split have run their detections.
    roster_lists(&server, &victim_public);
    let closed_log = server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("pane-close-recorder")
        .join("closed.log");
    let closed = || -> Vec<String> {
        std::fs::read_to_string(&closed_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["pane_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    assert_eq!(closed(), Vec::<String>::new(), "nothing has closed yet");

    // One tmux invocation closes the pane and splits another: two hooks, two detections.
    std::fs::write(&arm, "").unwrap();
    server.tmux_ok(&["kill-pane", "-t", &victim, ";", "split-window", "-t", &pane]);
    assert!(
        wait_for(|| !closed().is_empty(), HOOK),
        "the close was never reported"
    );
    // Long enough for a second report to have landed too.
    std::thread::sleep(SETTLE);
    assert!(
        !tmp.path().join("gave-up").exists(),
        "the first write was let go by the clock before the second arrived; this run proves nothing"
    );
    assert_eq!(
        closed(),
        vec![victim_public],
        "the pane must be reported by one detection, once"
    );
}

/// A plugin that keeps *every* `pane.closed` and `pane.exited` delivery, one context per line each.
const PANE_END_RECORDER: &str = r#"id = "pane-end-recorder"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Test plugin: keeps every pane.closed and pane.exited delivery."

[[events]]
on = "pane.closed"
command = ["sh", "-c", "printf '%s\n' \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$HERDR_PLUGIN_STATE_DIR/closed.log\""]

[[events]]
on = "pane.exited"
command = ["sh", "-c", "printf '%s\n' \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$HERDR_PLUGIN_STATE_DIR/exited.log\""]
"#;

/// A server with the plugin that keeps every close and every exit linked.
fn server_recording_pane_ends() -> (Server, tempfile::TempDir) {
    server_recording_pane_ends_with(ServerOptions::default())
}

/// [`server_recording_pane_ends`] for a server started with `options`.
fn server_recording_pane_ends_with(options: ServerOptions) -> (Server, tempfile::TempDir) {
    let server = Server::start_with(options);
    server.write_config(CONFIG);
    let recorder = tempfile::tempdir().unwrap();
    std::fs::write(recorder.path().join("herdr-plugin.toml"), PANE_END_RECORDER).unwrap();
    server.ok(
        &["plugin", "link", recorder.path().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    server.source_plugin();
    (server, recorder)
}

/// The pane ids the recorder was handed in `log` (`closed.log` or `exited.log`), one for each
/// delivery.
fn recorded_ids(server: &Server, log: &str) -> Vec<String> {
    let path = server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("pane-end-recorder")
        .join(log);
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|context| context["pane_id"].as_str().map(str::to_string))
        .collect()
}

/// How many times each of the races below is run: they are races between processes that tmux
/// starts, so one round says little and ten say whether the order is kept or only usually is.
const ROUNDS: usize = 10;

/// The last pane of a window exits. tmux fires `pane-exited` (whose job refreshes the pane roster)
/// and `window-unlinked` (whose job runs a `pane.closed` detection) for it, and the two jobs race:
/// a detection that lists the panes after the exit and writes the roster before the refresh does
/// reported the pane closed, in three to eight rounds of ten. An exit is never a close.
#[test]
fn the_last_pane_of_a_window_exiting_is_never_reported_closed() {
    let (server, _recorder) = server_recording_pane_ends();
    let mut exited = Vec::new();
    for round in 0..ROUNDS {
        let raw = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
        let public = server.public_pane_id(&raw);
        roster_lists(&server, &public);

        server.tmux_ok(&["send-keys", "-t", &raw, "exit", "Enter"]);

        assert!(
            wait_for(
                || recorded_ids(&server, "exited.log").contains(&public),
                HOOK
            ),
            "round {round}: pane.exited never arrived for {public}"
        );
        exited.push(public);
    }
    // Long enough for a second job to have finished too.
    std::thread::sleep(SETTLE);

    let closed = recorded_ids(&server, "closed.log");
    let reported: Vec<&String> = exited.iter().filter(|id| closed.contains(id)).collect();
    assert!(
        reported.is_empty(),
        "{} of {ROUNDS} exited panes were also reported closed: {reported:?}",
        reported.len()
    );
    // Whoever dropped a pane from the roster took its mark off.
    assert_eq!(
        server.global_opt("@agent_wizard_exited"),
        "",
        "the marks of panes that are gone were not taken off"
    );
}

/// One pane is killed in the same tmux invocation that makes another pane's shell exit (each in a
/// window that survives). The kill is a close and is reported once, whichever of the detections
/// that the hooks of the two fire gets to it, and the exit is never one.
#[test]
fn a_kill_beside_an_exit_is_reported_closed_once_and_the_exit_never() {
    let (server, _recorder) = server_recording_pane_ends();
    let (mut killed, mut exiting) = (Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        // Two windows of two panes: the pane that exits and the pane that is killed each leave
        // a pane behind, so neither is the last of its window.
        let second_pane = || {
            let first =
                server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
            server.tmux_ok(&["split-window", "-d", "-t", &first, "-P", "-F", "#{pane_id}"])
        };
        let (killed_raw, exiting_raw) = (second_pane(), second_pane());
        let (killed_public, exiting_public) = (
            server.public_pane_id(&killed_raw),
            server.public_pane_id(&exiting_raw),
        );
        roster_lists(&server, &killed_public);
        roster_lists(&server, &exiting_public);

        server.tmux_ok(&[
            "send-keys",
            "-t",
            &exiting_raw,
            "exit",
            "Enter",
            ";",
            "kill-pane",
            "-t",
            &killed_raw,
        ]);

        assert!(
            wait_for(
                || recorded_ids(&server, "exited.log").contains(&exiting_public),
                HOOK
            ),
            "round {round}: pane.exited never arrived for {exiting_public}"
        );
        killed.push(killed_public);
        exiting.push(exiting_public);
    }
    for id in &killed {
        assert!(
            wait_for(|| recorded_ids(&server, "closed.log").contains(id), HOOK),
            "the kill of {id} was never reported"
        );
    }
    // Long enough for a second report of a kill, or a report of an exit, to have landed.
    std::thread::sleep(SETTLE);

    let closed = recorded_ids(&server, "closed.log");
    let twice: Vec<&String> = killed
        .iter()
        .filter(|id| closed.iter().filter(|c| c == id).count() != 1)
        .collect();
    assert!(
        twice.is_empty(),
        "kills not reported exactly once: {twice:?}\nall reports: {closed:?}"
    );
    let reported: Vec<&String> = exiting.iter().filter(|id| closed.contains(id)).collect();
    assert!(
        reported.is_empty(),
        "{} of {ROUNDS} exited panes were reported closed: {reported:?}",
        reported.len()
    );
}

/// A `tmux`, armed once `armed` exists, that makes the first roster write of the `pane.closed`
/// detection wait (in `d0-reached`) until the `pane.exited` handler has read everything it writes
/// the roster from and come to its own first write (`x-ready`), and holds that write of the handler
/// until the detection's has landed (`d0-wrote`): the order in which the detection that writes the
/// first roster lists a pane while it is alive, the pane exits, and the detection's write lands
/// between the exit handler's reads and its write. The two are told apart by the command line of
/// the process that runs the `tmux`. Either waits ten seconds at most and says in `gave-up` when
/// it ran out of time, so that a test cannot pass for a hold that never held.
fn tmux_that_lets_the_first_roster_land_between_an_exits_reads_and_its_write(
    dir: &Path,
) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-landing-the-first-roster-between-an-exit-and-its-write");
    write_script(
        &wrapper,
        &format!(
            "o='{o}'\ncase \"$*\" in\n  *\"set-option -g @agent_wizard_panes\"*)\n    if [ -e \"$o/armed\" ]; then\n      case \"$(ps -o command= -p $PPID)\" in\n        *\"plugin event pane.exited\"*)\n          if mkdir \"$o/x-held\" 2>/dev/null; then\n            : > \"$o/x-ready\"\n            i=0\n            while [ ! -e \"$o/d0-wrote\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n            [ -e \"$o/d0-wrote\" ] || : > \"$o/gave-up\"\n          fi ;;\n        *\"plugin event pane.closed\"*)\n          if mkdir \"$o/d0-held\" 2>/dev/null; then\n            : > \"$o/d0-reached\"\n            i=0\n            while [ ! -e \"$o/x-ready\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n            [ -e \"$o/x-ready\" ] || : > \"$o/gave-up\"\n            '{real}' \"$@\"; status=$?; : > \"$o/d0-wrote\"; exit $status\n          fi ;;\n      esac\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            o = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// The first roster is written by the detection that the split of a pane fires, and the pane can
/// exit after that detection has listed it and before it has written: its exit handler finds a
/// roster that is not there yet, writes its own, and then finds that the detection's write has
/// landed first, with the exited pane in it. Marking an exit only once a roster exists left that
/// pane unmarked for the exit handler's second look, which reported it closed; an exit is marked
/// whether or not there is a roster, and the roster that is written late is not what the exited
/// pane comes back to.
#[test]
fn a_pane_that_exits_while_the_first_roster_is_being_written_is_not_reported_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper =
        tmux_that_lets_the_first_roster_land_between_an_exits_reads_and_its_write(tmp.path());
    let (server, _recorder) = server_recording_pane_ends_with(ServerOptions {
        env: vec![("TAW_TMUX_BIN".to_string(), wrapper.display().to_string())],
        ..ServerOptions::default()
    });
    assert_eq!(
        server.global_opt("@agent_wizard_panes_generation"),
        "",
        "a roster was written already, so there is no first one to land late"
    );
    let pane = server.pane();
    std::fs::write(tmp.path().join("armed"), "").unwrap();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let victim = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let terminal = split["pane"]["terminal_id"].as_str().unwrap().to_string();
    // The detection has listed the pane, and holds its write.
    assert!(
        wait_for(|| tmp.path().join("d0-reached").exists(), HOOK),
        "the detection never came to its write"
    );

    server.tmux_ok(&["send-keys", "-t", &terminal, "exit", "Enter"]);

    assert!(
        wait_for(
            || recorded_ids(&server, "exited.log").contains(&victim),
            HOOK
        ),
        "pane.exited never arrived for {victim}"
    );
    assert!(
        wait_for(|| tmp.path().join("d0-wrote").exists(), HOOK),
        "the detection's write never landed"
    );
    // Long enough for the exit handler to have read again and reported, if it was going to.
    std::thread::sleep(SETTLE);
    assert!(
        !tmp.path().join("gave-up").exists(),
        "a hold ran out of time, so the order was not the one this test is about"
    );
    assert_eq!(
        recorded_ids(&server, "closed.log"),
        Vec::<String>::new(),
        "an exit was reported as a close (the roster holds {:?})",
        server.global_opt("@agent_wizard_panes")
    );
}

/// A `tmux`, armed once `armed` exists, that makes the first `pane.closed` detection wait before it
/// lists the live panes (`d-reached`) until `release` exists, and holds the first roster write of
/// the `pane.exited` handler until the detection has written its own (`d-wrote`): the order in which
/// a detection has read the roster, and whatever it reads before the live panes, when a pane exits
/// and the handler for its exit can do nothing yet. The two are told apart by the command line of
/// the process that runs the `tmux`. Either waits ten seconds at most and says in `gave-up` when it
/// ran out of time, so that a test cannot pass for a hold that never held.
fn tmux_that_holds_a_detection_before_it_lists_the_panes(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-a-detection-before-it-lists-the-panes");
    write_script(
        &wrapper,
        &format!(
            "o='{o}'\ncase \"$*\" in\n  *\" list-panes -a -F \"*)\n    if [ -e \"$o/armed\" ]; then\n      case \"$(ps -o command= -p $PPID)\" in\n        *\"plugin event pane.closed\"*)\n          if mkdir \"$o/d-held\" 2>/dev/null; then\n            : > \"$o/d-reached\"\n            i=0\n            while [ ! -e \"$o/release\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n            [ -e \"$o/release\" ] || : > \"$o/gave-up\"\n          fi ;;\n      esac\n    fi ;;\n  *\"set-option -g @agent_wizard_panes\"*)\n    if [ -e \"$o/armed\" ]; then\n      case \"$(ps -o command= -p $PPID)\" in\n        *\"plugin event pane.exited\"*)\n          if mkdir \"$o/x-held\" 2>/dev/null; then\n            i=0\n            while [ ! -e \"$o/d-wrote\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n            [ -e \"$o/d-wrote\" ] || : > \"$o/gave-up\"\n          fi ;;\n        *\"plugin event pane.closed\"*)\n          if [ -e \"$o/d-held\" ]; then\n            '{real}' \"$@\"; status=$?; : > \"$o/d-wrote\"; exit $status\n          fi ;;\n      esac\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            o = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// A detection reads the roster, the live panes and the marks of exits, and a pane that exited is
/// gone from the live panes and in the marks. The marks are dependable only when they are read
/// after the live panes: one read before them can be from before the exit, with the panes read after
/// it, and the pane is then gone and not marked. A `tmux` that holds the detection before it lists
/// the panes while the pane exits shows which order it reads them in, with the exit handler held
/// back from writing the roster first, which would hide it.
#[test]
fn a_detection_reads_the_marks_of_exits_after_the_live_panes() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmux_that_holds_a_detection_before_it_lists_the_panes(tmp.path());
    let (server, _recorder) = server_recording_pane_ends_with(ServerOptions {
        env: vec![("TAW_TMUX_BIN".to_string(), wrapper.display().to_string())],
        ..ServerOptions::default()
    });
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let victim = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let terminal = split["pane"]["terminal_id"].as_str().unwrap().to_string();
    // The roster knows the pane that is about to exit.
    roster_lists(&server, &victim);

    // Another split fires a detection, which has read the roster when it is held before it lists.
    std::fs::write(tmp.path().join("armed"), "").unwrap();
    server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    assert!(
        wait_for(|| tmp.path().join("d-reached").exists(), HOOK),
        "the detection never came to listing the panes"
    );

    // The pane exits while it waits, and the hook marks it.
    server.tmux_ok(&["send-keys", "-t", &terminal, "exit", "Enter"]);
    assert!(
        wait_for(
            || server
                .global_opt("@agent_wizard_exited")
                .split_whitespace()
                .any(|mark| mark == terminal),
            HOOK
        ),
        "the exit was never marked (the marks hold {:?})",
        server.global_opt("@agent_wizard_exited")
    );
    std::fs::write(tmp.path().join("release"), "").unwrap();

    assert!(
        wait_for(
            || recorded_ids(&server, "exited.log").contains(&victim),
            HOOK
        ),
        "pane.exited never arrived for {victim}"
    );
    assert!(
        wait_for(|| tmp.path().join("d-wrote").exists(), HOOK),
        "the held detection never wrote the roster"
    );
    // Long enough for a report to have landed, if there was going to be one.
    std::thread::sleep(SETTLE);
    assert!(
        !tmp.path().join("gave-up").exists(),
        "a hold ran out of time, so the order was not the one this test is about"
    );
    assert_eq!(
        recorded_ids(&server, "closed.log"),
        Vec::<String>::new(),
        "an exit was reported as a close (the roster holds {:?})",
        server.global_opt("@agent_wizard_panes")
    );
}

/// A `tmux`, armed once `armed` exists, that makes the first roster write of the `pane.closed`
/// detection wait until a `pane.exited` handler has written (`x-wrote`): the order in which the
/// handler for an exit takes the roster before the detection that the kill fired beside it. The two
/// are told apart by the command line of the process that runs the `tmux`. The detection waits ten
/// seconds at most and says in `gave-up` when it ran out of time, so that a test cannot pass for a
/// hold that never held.
fn tmux_that_lets_an_exit_take_the_roster_before_a_detection(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-letting-an-exit-take-the-roster-first");
    write_script(
        &wrapper,
        &format!(
            "o='{o}'\ncase \"$*\" in\n  *\"set-option -g @agent_wizard_panes\"*)\n    if [ -e \"$o/armed\" ]; then\n      case \"$(ps -o command= -p $PPID)\" in\n        *\"plugin event pane.closed\"*)\n          if mkdir \"$o/d-held\" 2>/dev/null; then\n            i=0\n            while [ ! -e \"$o/x-wrote\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n            [ -e \"$o/x-wrote\" ] || : > \"$o/gave-up\"\n          fi ;;\n        *\"plugin event pane.exited\"*)\n          '{real}' \"$@\"; status=$?; : > \"$o/x-wrote\"; exit $status ;;\n      esac\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            o = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// A pane is killed in the same tmux invocation that makes another pane's shell exit. The handler
/// for the exit takes the roster too, and when it takes it before the detection that the kill fired,
/// it is the one that finds the killed pane gone: it reports that pane closed, as the detection
/// would have, and not the pane that exited. A handler that only recorded the panes dropped the kill
/// from the roster, and the detection after it found nothing to report.
#[test]
fn a_kill_that_lands_with_an_exit_is_reported_by_the_exits_own_take_of_the_roster() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = tmux_that_lets_an_exit_take_the_roster_before_a_detection(tmp.path());
    let (server, _recorder) = server_recording_pane_ends_with(ServerOptions {
        env: vec![("TAW_TMUX_BIN".to_string(), wrapper.display().to_string())],
        ..ServerOptions::default()
    });
    let pane = server.pane();
    let split = |_: usize| {
        server.ok(
            &["pane", "split", &server.public_pane_id(&pane)],
            "cli:pane:split",
            "pane_split",
        )
    };
    let (killed, exiting) = (split(0), split(1));
    let (killed_raw, exiting_raw) = (
        killed["pane"]["terminal_id"].as_str().unwrap().to_string(),
        exiting["pane"]["terminal_id"].as_str().unwrap().to_string(),
    );
    let (killed_public, exiting_public) = (
        killed["pane"]["pane_id"].as_str().unwrap().to_string(),
        exiting["pane"]["pane_id"].as_str().unwrap().to_string(),
    );
    roster_lists(&server, &killed_public);
    roster_lists(&server, &exiting_public);

    std::fs::write(tmp.path().join("armed"), "").unwrap();
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &exiting_raw,
        "exit",
        "Enter",
        ";",
        "kill-pane",
        "-t",
        &killed_raw,
    ]);

    assert!(
        wait_for(
            || recorded_ids(&server, "closed.log").contains(&killed_public),
            HOOK
        ),
        "the kill was never reported"
    );
    // Long enough for a second report of the kill, or one of the exit, to have landed.
    std::thread::sleep(SETTLE);
    assert!(
        tmp.path().join("x-wrote").exists(),
        "the exit's handler never wrote the roster, so the order was not the one this test is about"
    );
    assert!(
        !tmp.path().join("gave-up").exists(),
        "the detection ran out of time waiting, so the order was not the one this test is about"
    );
    assert_eq!(
        recorded_ids(&server, "closed.log"),
        vec![killed_public],
        "the kill is reported once and the exit of {exiting_public} not at all"
    );
}

/// A `tmux`, first on `PATH` for the plugin script of a source (`bin/tmux` in `dir`), armed once
/// `armed` exists: it stops the script, for as long as `release` is missing, right after the second
/// entry of the `pane-exited` hook has been taken off (`stopped` says when it is stopped), and says in
/// `gave-up` when it ran out of its ten seconds. The hook holds two entries of the plugin's (the mark
/// and the job that fires `pane.exited`), so that is the moment at which a source that takes every
/// entry off before it puts any back has the hook with none, and one that puts the new entries in
/// first has the new two.
fn tmux_that_stops_a_source_after_its_second_pane_exited_unset(dir: &Path) {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    write_script(
        &dir.join("bin").join("tmux"),
        &format!(
            "o='{o}'\n'{real}' \"$@\"\nstatus=$?\nif [ -e \"$o/armed\" ]; then\n  case \"$*\" in\n    *\"set-hook -gu pane-exited[\"*)\n      n=$(cat \"$o/unsets\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$o/unsets\"\n      if [ $n -eq 2 ]; then\n        : > \"$o/stopped\"\n        i=0\n        while [ ! -e \"$o/release\" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n        [ -e \"$o/release\" ] || : > \"$o/gave-up\"\n      fi ;;\n  esac\nfi\nexit $status\n",
            o = dir.display(),
            real = real.display(),
        ),
    );
}

/// The environment of a server whose shells are typed into: a home of its own and no history file,
/// since a shell saves what it was typed when the server is killed, to `$HISTFILE`, which is
/// `~/.bash_history` where nothing says otherwise.
fn private_shells(home: &Path) -> Vec<(String, String)> {
    std::fs::create_dir_all(home).unwrap();
    vec![
        ("HISTFILE".to_string(), "/dev/null".to_string()),
        ("HOME".to_string(), home.display().to_string()),
    ]
}

/// Sources the plugin script with the `tmux` of `bin_dir` first on its `PATH`.
fn source_plugin_through(server: &Server, bin_dir: &Path) {
    server.tmux_ok(&[
        "run-shell",
        &format!(
            "PATH='{}':\"$PATH\" TAW_BIN='{}' bash '{}'",
            bin_dir.display(),
            taw_bin(),
            common::plugin_script().display()
        ),
    ]);
}

/// A source of the plugin over itself (an upgrade of taw, a reload of tmux.conf) must not take
/// every entry of every hook off before it puts any back, or an exit that came in between had
/// neither its mark nor its `pane.exited` job: lost, or, for the last pane of a window, reported as
/// a close (the mark is what tells a detection that the pane that is gone exited). The new entries
/// are put in first now and the old ones taken off after, so that a hook is never without one.
/// Stopped between the two, the source has the hook with its new entries in, and the exit is heard
/// and is no close.
#[test]
fn an_exit_while_the_plugin_is_sourced_over_itself_is_marked_and_heard() {
    let tmp = tempfile::tempdir().unwrap();
    tmux_that_stops_a_source_after_its_second_pane_exited_unset(tmp.path());
    let (server, _recorder) = server_recording_pane_ends_with(ServerOptions {
        env: private_shells(&tmp.path().join("home")),
        ..ServerOptions::default()
    });
    let raw = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
    let public = server.public_pane_id(&raw);
    roster_lists(&server, &public);
    std::fs::write(tmp.path().join("armed"), "").unwrap();

    let heard = std::thread::scope(|scope| {
        let sourcing = scope.spawn(|| source_plugin_through(&server, &tmp.path().join("bin")));
        assert!(
            wait_for(|| tmp.path().join("stopped").exists(), SOURCE),
            "the source never took the entries of pane-exited off"
        );

        server.tmux_ok(&["send-keys", "-t", &raw, "exit", "Enter"]);

        let heard = wait_for(
            || recorded_ids(&server, "exited.log").contains(&public),
            HOOK,
        );
        std::fs::write(tmp.path().join("release"), "").unwrap();
        sourcing.join().unwrap();
        heard
    });
    // Long enough for a detection to have reported the exit as a close, if it was going to.
    std::thread::sleep(SETTLE);

    assert!(
        !tmp.path().join("gave-up").exists(),
        "the stopped source ran out of time, so the order was not the one this test is about"
    );
    assert!(heard, "pane.exited was lost: the hook had no entries");
    assert_eq!(
        recorded_ids(&server, "closed.log"),
        Vec::<String>::new(),
        "an exit was reported as a close: it was not marked"
    );
}

/// How many times a pane exits during a source of the plugin over itself, each time a little further
/// into it: the source takes some 700 ms, and every part of it is somewhere in the spread.
const SOURCE_ROUNDS: u64 = 12;

/// The exit of a pane at each moment of a source of the plugin over itself, as a person's reload
/// meets one: heard, and never a close. (Each round is a race between processes that tmux starts, so
/// one says little: a source that strips before it adds lost the exit in five rounds of thirteen and
/// reported it closed in three.)
#[test]
fn a_pane_that_exits_at_any_moment_of_a_source_is_heard_and_is_not_a_close() {
    let home = tempfile::tempdir().unwrap();
    let (server, _recorder) = server_recording_pane_ends_with(ServerOptions {
        env: private_shells(home.path()),
        ..ServerOptions::default()
    });
    let (mut lost, mut closed) = (Vec::new(), Vec::new());
    for round in 0..SOURCE_ROUNDS {
        // The pane is not the last of its window, and the next
        // window's creation is what fires a detection afterwards.
        let first = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
        let raw = server.tmux_ok(&["split-window", "-d", "-t", &first, "-P", "-F", "#{pane_id}"]);
        let public = server.public_pane_id(&raw);
        roster_lists(&server, &public);

        std::thread::scope(|scope| {
            let sourcing = scope.spawn(|| server.source_plugin());
            std::thread::sleep(Duration::from_millis(75 * round));
            server.tmux_ok(&["send-keys", "-t", &raw, "exit", "Enter"]);
            sourcing.join().unwrap();
        });
        // A detection anywhere, as the next window would fire it.
        let later = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
        roster_lists(&server, &server.public_pane_id(&later));
        if !wait_for(
            || recorded_ids(&server, "exited.log").contains(&public),
            HOOK,
        ) {
            lost.push(public.clone());
        }
        std::thread::sleep(SETTLE);
        if recorded_ids(&server, "closed.log").contains(&public) {
            closed.push(public);
        }
    }
    assert!(
        lost.is_empty() && closed.is_empty(),
        "of {SOURCE_ROUNDS} exits during a source, pane.exited was lost for {lost:?} and a close \
         was reported for {closed:?}"
    );
}

#[test]
fn an_unhooked_event_costs_nothing_and_an_unknown_name_is_rejected() {
    // No plugin linked: the hooks are installed but every fire short-circuits before it
    // reads tmux, which is what makes leaving `window-layout-changed` hooked affordable.
    let server = Server::start();
    server.write_config(CONFIG);
    server.source_plugin();

    let result = server.ok(
        &["plugin", "event", "layout.updated"],
        "cli:plugin:event",
        "plugin_event",
    );
    assert_eq!(result["launched"], 0);
    assert_eq!(result["subscribed"], false);
    // No `--tab` was given and none was needed: with nothing subscribed there is no
    // payload to build, so a missing identifier is not even an error.
    assert!(result.get("payload").is_none(), "{result}");

    let (code, envelope) = server.taw(&["plugin", "event", "nope.invented"]);
    assert_eq!(code, 1);
    assert_eq!(envelope["error"]["code"], "invalid_argument");

    // An event this bridge does not own names the verb that does, rather than emitting a
    // second copy of something a verb already fires.
    let (code, envelope) = server.taw(&["plugin", "event", "pane.updated"]);
    assert_eq!(code, 1);
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("taw pane rename"),
        "{envelope}"
    );
}

#[test]
fn a_missing_identifier_is_reported_once_a_plugin_is_actually_subscribed() {
    let server = server();
    let (code, envelope) = server.taw(&["plugin", "event", "tab.closed", "--tab", "@1"]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--workspace"),
        "{envelope}"
    );
    // A tmux format that expanded to nothing is the same case, and must not be passed
    // through as if it were an id.
    let (code, envelope) = server.taw(&[
        "plugin",
        "event",
        "tab.closed",
        "--tab",
        "@1",
        "--workspace",
        "",
    ]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument");

    // Belt: the binary the hooks were built against is the one under test.
    assert!(PathBuf::from(taw_bin()).exists());
}

#[test]
fn a_pane_killed_with_a_tmux_key_fires_pane_closed_with_its_own_id() {
    // The gap this closes. `prefix x` runs `kill-pane`, which never passes through `taw`,
    // and tmux's `after-kill-pane` names no pane — so the id is recovered by diffing the
    // live panes against the previous set.
    let server = server();
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let victim_public = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let victim_raw = split["pane"]["terminal_id"].as_str().unwrap().to_string();

    roster_lists(&server, &victim_public);
    // Raw tmux, exactly as `prefix x` would.
    server.tmux_ok(&["kill-pane", "-t", &victim_raw]);
    let closed = recorded(&server, "pane.closed");
    assert_eq!(closed["pane_id"], victim_public.as_str());
    assert_eq!(
        closed["workspace_id"],
        victim_public.split_once(':').unwrap().0
    );
    // The bare id still populates the record slot, which is where HERDR_PANE_ID comes from.
    assert_eq!(closed["pane"]["pane_id"], victim_public.as_str());
}

#[test]
fn one_close_fires_pane_closed_once_even_though_two_hooks_watch_for_it() {
    // `after-kill-pane` and `after-split-window` both run the detection, so a close
    // followed by a split must not report the same pane twice — the roster is rewritten
    // by every pass.
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    let first = server.ok(
        &["pane", "split", &public, "--direction", "right"],
        "cli:pane:split",
        "pane_split",
    );
    let victim = first["pane"]["terminal_id"].as_str().unwrap().to_string();
    let victim_public = first["pane"]["pane_id"].as_str().unwrap().to_string();

    roster_lists(&server, &victim_public);
    server.tmux_ok(&["kill-pane", "-t", &victim]);
    let closed = recorded(&server, "pane.closed");
    assert_eq!(closed["pane_id"], victim_public.as_str());

    // Forget it, then make several more hooks fire. None of them may report the pane
    // that closed before — it is no longer in the roster.
    forget(&server, "pane.closed");
    server.ok(
        &["pane", "split", &public, "--direction", "down"],
        "cli:pane:split",
        "pane_split",
    );
    server.ok(
        &["tab", "create", "--label", "extra"],
        "cli:tab:create",
        "tab_created",
    );
    // Give the hooks their two asynchronous hops before concluding nothing arrived.
    assert!(
        !common::wait_for(|| event_file(&server, "pane.closed").exists(), HOOK),
        "pane.closed fired again for a pane that had already been reported"
    );
}

#[test]
fn pane_closed_reports_nothing_the_first_time_rather_than_a_backlog() {
    // With no roster yet, the first detection seeds it. Otherwise a plugin installed after
    // a day's work would be handed every pane that had closed in the meantime.
    let server = server();
    let pane = server.pane();
    server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    // The splits above already primed the roster through their own hook, so clear it to
    // get back to the un-primed state a fresh install starts from.
    server.tmux_ok(&["set-option", "-gu", "@agent_wizard_panes"]);
    forget(&server, "pane.closed");

    let seeded = server.ok(
        &["plugin", "event", "pane.closed"],
        "cli:plugin:event",
        "plugin_event",
    );
    assert_eq!(
        seeded["closed"].as_array().unwrap().len(),
        0,
        "the first detection must seed, not report: {seeded}"
    );
    assert_ne!(
        server.global_opt("@agent_wizard_panes"),
        "",
        "the roster should have been seeded: {seeded}"
    );
    assert!(!event_file(&server, "pane.closed").exists());
}

#[test]
fn an_unsubscribed_server_keeps_no_pane_roster_at_all() {
    // Nothing subscribed: the detection must not leave a roster behind, or the first
    // plugin to subscribe would be handed everything that closed while nobody listened.
    let server = Server::start();
    server.write_config(CONFIG);
    server.source_plugin();
    let pane = server.pane();

    server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    let result = server.ok(
        &["plugin", "event", "pane.closed"],
        "cli:plugin:event",
        "plugin_event",
    );
    assert_eq!(result["subscribed"], false);
    assert_eq!(
        server.global_opt("@agent_wizard_panes"),
        "",
        "an unsubscribed detection must leave no roster: {result}"
    );
}

/// The roster is written with a generation, and the two go together: when the last plugin that hooks
/// `pane.closed` has gone, the next detection forgets both, so that a plugin that subscribes later
/// starts from nothing, as the first one did, instead of being handed what closed while nobody listened.
#[test]
fn the_roster_and_its_generation_are_forgotten_together_when_the_last_plugin_goes() {
    let server = server();
    let pane = server.pane();
    let split = server.ok(
        &["pane", "split", &server.public_pane_id(&pane)],
        "cli:pane:split",
        "pane_split",
    );
    roster_lists(&server, split["pane"]["pane_id"].as_str().unwrap());
    assert_ne!(
        server.global_opt("@agent_wizard_panes_generation"),
        "",
        "a roster that was written has a generation"
    );
    // A pane that exited, whose mark is there for the roster that is about to go.
    server.tmux_ok(&["set-option", "-g", "@agent_wizard_exited", "%99 "]);

    let (code, envelope) = server.taw(&["plugin", "unlink", "echo-plugin"]);
    assert_eq!(code, 0, "{envelope}");
    let result = server.ok(
        &["plugin", "event", "pane.closed"],
        "cli:plugin:event",
        "plugin_event",
    );
    assert_eq!(result["subscribed"], false);
    assert_eq!(
        server.global_opt("@agent_wizard_panes"),
        "",
        "the roster outlived the plugin: {result}"
    );
    assert_eq!(
        server.global_opt("@agent_wizard_panes_generation"),
        "",
        "the generation outlived the roster: {result}"
    );
    assert_eq!(
        server.global_opt("@agent_wizard_exited"),
        "",
        "the marks outlived the roster: {result}"
    );
}

/// A plugin that keeps *every* `workspace.closed` delivery, one context per line — the echo
/// plugin keeps only the last, which cannot tell one event from two.
const CLOSE_RECORDER: &str = r#"id = "close-recorder"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Test plugin: keeps every workspace.closed delivery."

[[events]]
on = "workspace.closed"
command = ["sh", "-c", "printf '%s\\n' \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$HERDR_PLUGIN_STATE_DIR/closed.log\""]
"#;

/// `workspace close --group` (herdr v0.9.1, #2874) closes the workspace and its worktree
/// workspaces, and each is one `workspace.closed` — from tmux's `session-closed` hook, once
/// per session, exactly as for a hand `tmux kill-session`. A refused close reports nothing.
#[test]
fn closing_a_workspace_group_reports_workspace_closed_once_for_each_session() {
    let server = server();
    let recorder = tempfile::tempdir().unwrap();
    std::fs::write(recorder.path().join("herdr-plugin.toml"), CLOSE_RECORDER).unwrap();
    server.ok(
        &["plugin", "link", recorder.path().to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    let closed_log = server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("close-recorder")
        .join("closed.log");
    let closed_ids = || -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_to_string(&closed_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["workspace_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        ids.sort();
        ids
    };

    // The server's first workspace becomes a repository with one worktree workspace cut
    // from it.
    let git = |args: &[&str]| {
        let status = taw::git::command()
            .arg("-C")
            .arg(server.work_dir())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(server.work_dir().join("README.md"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=Test",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        "first",
    ]);
    let checkouts = tempfile::tempdir().unwrap();
    server.write_config(&format!(
        "{CONFIG}\n[worktrees]\ndirectory = \"{}\"\n",
        checkouts.path().display()
    ));
    let primary = server.ok(
        &["workspace", "list"],
        "cli:workspace:list",
        "workspace_list",
    )["workspaces"][0]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let created = server.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            &primary,
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let linked = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Refused: nothing closes, so nothing is reported.
    let (code, env) = server.taw(&["workspace", "close", &primary]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "workspace_group_close_required");
    std::thread::sleep(SETTLE);
    assert_eq!(closed_ids(), Vec::<String>::new());

    // With --group: both close, and each is reported once.
    server.ok(
        &["workspace", "close", &primary, "--group"],
        "cli:workspace:close",
        "ok",
    );
    assert!(
        wait_for(|| closed_ids().len() >= 2, HOOK),
        "expected two workspace.closed events, got {:?}",
        closed_ids()
    );
    std::thread::sleep(SETTLE);
    let mut expected = vec![primary, linked];
    expected.sort();
    assert_eq!(closed_ids(), expected, "one event per closed session");
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): the tests of this file that make a repository, run again
/// with each of the variables pointing into a decoy, pass and leave it byte for byte as it was
/// (tests/common/git_decoy.rs).
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &["closing_a_workspace_group_reports_workspace_closed_once_for_each_session"],
        "leave_the_one_gits_variables_point_at",
    );
}
