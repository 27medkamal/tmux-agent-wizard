// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Integration tests for session snapshot/restore against a real (throwaway) tmux server.
//! Never touches the default tmux server, and never writes outside tempdirs and this
//! throwaway server.

mod common;

use common::shell_env::ShellHome;
use taw::config::Config;
use taw::paths::Paths;
use taw::session::{restore, snapshot};
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop. See
/// tests/tmux_client.rs for why the socket name includes both the pid and a per-`Server`
/// counter, and why `-f /dev/null` is required (a fresh `-L` socket still loads the host's
/// own tmux.conf otherwise).
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    /// Starts the server with its first session ("alpha") already sized `200x50` —
    /// `restore::apply` hardcodes that size on the `new-session` it runs to recreate a
    /// killed session (detached sessions need an explicit size for `select-layout` to
    /// apply), so every session created in these tests uses the same size: that guarantees
    /// a captured `window_layout` string's *geometry* (every leaf's `WxH,X,Y`, and the
    /// split-tree shape) round-trips through restore's `select-layout` rather than being
    /// rescaled to a different window size. The string is not expected to round-trip
    /// byte-for-byte, even so: both the leading checksum and each leaf's trailing number
    /// are derived from tmux's real, global, never-reused pane ids, which are necessarily
    /// different once a pane has been killed and recreated — see `normalize_layout` below.
    ///
    /// Also disables `exit-empty` (tmux's default of killing the whole server once it has
    /// no sessions left): several tests here kill "alpha" — the server's only session at
    /// that point — specifically so restore has something to recreate, which would
    /// otherwise take the throwaway server down with it.
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-session-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        home.start_server(
            &t,
            &["-d", "-s", "alpha", "-x", "200", "-y", "50", "/bin/sh"],
        );
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
        t.run_ok(&["set-option", "-g", "exit-empty", "off"])
            .unwrap();
        Server(t, home)
    }
}

/// Strips the parts of a `#{window_layout}` string that tmux derives from real pane ids
/// (global, server-wide, never reused) and from where the focus was, leaving only the
/// geometric/structural part restore is actually responsible for reproducing: the split
/// tree and every cell's geometry. Both formats: the older text one (tmux before 3.8) loses
/// its leading checksum and each leaf's trailing id, leaving every leaf's `WxH,X,Y` and the
/// `{}`/`[]` shape; the JSON one (tmux 3.8) is parsed and keeps each cell's `t`, `w`, `h`,
/// `x`, `y` and `c`, and whether the cell floats (a floating cell carries a `z`, kept as
/// `floating: true`, not as its value), and loses the rest: the pane id (`I`), the focus keys
/// (`a`, `l`), and the pane and stacking numbers (`i`, and `z`'s value).
fn normalize_layout(layout: &str) -> String {
    if layout.trim_start().starts_with('{') {
        let json: serde_json::Value = serde_json::from_str(layout).unwrap_or_else(|err| {
            panic!("a JSON window layout that does not parse: {err}: {layout}")
        });
        return normalize_json_layout(&json).to_string();
    }
    let without_checksum = layout
        .split_once(',')
        .map(|(_, rest)| rest)
        .unwrap_or(layout);
    let leaf_id = regex::Regex::new(r"(\d+x\d+,\d+,\d+),\d+").unwrap();
    leaf_id.replace_all(without_checksum, "$1").into_owned()
}

/// [`normalize_layout`] for a parsed JSON layout: the document's version and tree, and in each cell
/// only `t`, `w`, `h`, `x`, `y`, whether it floats and, for a split, its cells `c`.
fn normalize_json_layout(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) if map.contains_key("L") => {
            let mut out = serde_json::Map::new();
            for key in ["V", "L"] {
                if let Some(v) = map.get(key) {
                    out.insert(key.to_string(), normalize_json_layout(v));
                }
            }
            Value::Object(out)
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for key in ["t", "w", "h", "x", "y"] {
                if let Some(v) = map.get(key) {
                    out.insert(key.to_string(), v.clone());
                }
            }
            if map.contains_key("z") {
                out.insert("floating".to_string(), Value::Bool(true));
            }
            if let Some(Value::Array(cells)) = map.get("c") {
                out.insert(
                    "c".to_string(),
                    Value::Array(cells.iter().map(normalize_json_layout).collect()),
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// A restored window has new pane ids and its own focus, and nothing else of the layout differs:
/// the JSON layout of tmux 3.8 compares by the tree, the geometry and which cells float, whatever
/// the ids, the focus keys and the stacking heights say (a text layout, the older format, the same by its geometry and its shape).
#[test]
fn layouts_compare_by_shape_and_geometry_in_both_formats() {
    let before = r#"{"V":2,"L":{"t":"h","w":200,"h":50,"x":0,"y":0,"c":[{"t":"p","w":100,"h":50,"x":0,"y":0,"i":0,"a":true,"I":"%1"},{"t":"v","w":99,"h":50,"x":101,"y":0,"c":[{"t":"p","w":99,"h":25,"x":101,"y":0,"i":1,"I":"%2"},{"t":"p","w":99,"h":24,"x":101,"y":26,"i":2,"l":true,"I":"%3"}]}]}}"#;
    let after = r#"{"V":2,"L":{"t":"h","w":200,"h":50,"x":0,"y":0,"c":[{"t":"p","w":100,"h":50,"x":0,"y":0,"i":0,"I":"%7"},{"t":"v","w":99,"h":50,"x":101,"y":0,"c":[{"t":"p","w":99,"h":25,"x":101,"y":0,"i":1,"a":true,"I":"%8"},{"t":"p","w":99,"h":24,"x":101,"y":26,"i":2,"I":"%9"}]}]}}"#;
    assert_eq!(normalize_layout(before), normalize_layout(after));
    // A floating cell carries a `z`: that it floats counts, how high it is stacked does not.
    let floating = r#"{"V":2,"L":{"t":"h","w":80,"h":24,"x":0,"y":0,"c":[{"t":"p","w":80,"h":24,"x":0,"y":0,"i":0,"I":"%1"},{"t":"p","w":20,"h":5,"x":10,"y":4,"i":1,"z":1,"I":"%2"}]}}"#;
    assert_eq!(
        normalize_layout(floating),
        normalize_layout(&floating.replacen(r#""z":1"#, r#""z":7"#, 1))
    );
    assert_ne!(
        normalize_layout(floating),
        normalize_layout(&floating.replacen(r#""z":1,"#, "", 1)),
        "a floating pane that came back tiled"
    );
    for (what, other) in [
        ("a width", after.replacen(r#""w":100"#, r#""w":99"#, 1)),
        ("a split's direction", after.replacen(r#""t":"v""#, r#""t":"h""#, 1)),
        ("a height", after.replacen(r#""h":25"#, r#""h":26"#, 1)),
        ("a y offset", after.replacen(r#""y":26"#, r#""y":25"#, 1)),
        ("an x offset", after.replacen(r#""x":101,"y":26"#, r#""x":102,"y":26"#, 1)),
        (
            "a missing pane",
            r#"{"V":2,"L":{"t":"h","w":200,"h":50,"x":0,"y":0,"c":[{"t":"p","w":100,"h":50,"x":0,"y":0,"I":"%7"}]}}"#.to_string(),
        ),
    ] {
        assert_ne!(normalize_layout(before), normalize_layout(&other), "{what}");
    }

    let old_before = "6ba0,200x50,0,0{100x50,0,0,1,99x50,101,0[99x25,101,0,2,99x24,101,26,3]}";
    let old_after = "ffff,200x50,0,0{100x50,0,0,7,99x50,101,0[99x25,101,0,8,99x24,101,26,9]}";
    assert_eq!(normalize_layout(old_before), normalize_layout(old_after));
    assert_ne!(
        normalize_layout(old_before),
        normalize_layout("6ba0,200x50,0,0{100x50,0,0,1,99x50,101,0[99x25,101,0,2,99x23,101,26,3]}")
    );
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// A `Paths` rooted entirely inside a tempdir — every field is independent of the real
/// `$TAW_CONFIG_DIR`/`$TAW_STATE_DIR`, so these tests never write outside it.
fn test_paths(dir: &std::path::Path) -> Paths {
    let plugins_dir = dir.join("plugins");
    Paths {
        config_dir: dir.join("config"),
        config_file: dir.join("config").join("config.toml"),
        state_dir: dir.join("state"),
        override_dir: dir.join("config").join("agent-detection"),
        remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
        sessions_dir: dir.join("state").join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: dir.join("state").join("logs"),
        manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
    }
}

/// Every window of `session_name`, sorted ascending by (the possibly-freshly-assigned)
/// `window_index` — restore does not try to preserve original tmux window indices (new
/// windows are created with `new-window -t <session>:`, letting tmux auto-number them), so
/// callers compare shape (name/pane-count/layout) in creation order, not by literal index.
fn windows_of(t: &Tmux, session_name: &str) -> Vec<taw::tmux::WindowInfo> {
    let session_id = t
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.session_name == session_name)
        .unwrap_or_else(|| panic!("session '{session_name}' not found"))
        .session_id;
    let mut windows: Vec<_> = t
        .list_windows()
        .unwrap()
        .into_iter()
        .filter(|w| w.session_id == session_id)
        .collect();
    windows.sort_by_key(|w| w.window_index);
    windows
}

fn poll_until(timeout: std::time::Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn snapshot_then_restore_recreates_a_killed_session_with_matching_shape() {
    let s = Server::start();
    let t = &s.0;

    // alpha: window "editor" (the free window, renamed), window "logs" (split into 2 panes).
    t.run_ok(&["rename-window", "-t", "alpha:0", "editor"])
        .unwrap();
    t.run_ok(&["new-window", "-d", "-t", "alpha:", "-n", "logs"])
        .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "alpha:logs"])
        .unwrap();

    // Select "logs", not window 0: restore has to *choose* the saved active window, and
    // with the first one active that choice is indistinguishable from doing nothing.
    t.run_ok(&["select-window", "-t", "alpha:logs"]).unwrap();

    // beta: window "shell". Two sessions, three windows, one split.
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();
    t.run_ok(&["rename-window", "-t", "beta:0", "shell"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());

    let snap = snapshot::take(t).unwrap();
    assert_eq!(snap.sessions.len(), 2);
    let alpha_snap = snap
        .sessions
        .iter()
        .find(|s| s.name == "alpha")
        .unwrap()
        .clone();
    assert_eq!(alpha_snap.windows.len(), 2);
    assert_eq!(alpha_snap.windows[0].name, "editor");
    assert_eq!(alpha_snap.windows[1].name, "logs");
    assert_eq!(alpha_snap.windows[1].panes.len(), 2);

    snapshot::save(&paths, "default", &snap).unwrap();

    // Kill "alpha" so restore has to recreate it; "beta" is left alone.
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
    assert!(
        !t.list_sessions()
            .unwrap()
            .iter()
            .any(|sess| sess.session_name == "alpha")
    );

    let cfg = Config::default();

    // Dry run: reports the plan, creates nothing.
    let dry = restore::apply(t, &cfg, &paths, "default", true).unwrap();
    assert!(
        dry.plan.iter().any(|l| l.contains("alpha")),
        "dry-run plan did not mention 'alpha': {:#?}",
        dry.plan
    );
    assert!(dry.created.is_empty());
    assert!(
        !t.list_sessions()
            .unwrap()
            .iter()
            .any(|sess| sess.session_name == "alpha"),
        "dry-run must not create anything"
    );

    // Real run: recreates alpha, leaves the already-existing beta untouched.
    let report = restore::apply(t, &cfg, &paths, "default", false).unwrap();
    assert_eq!(report.created, vec!["alpha".to_string()]);
    assert_eq!(report.skipped, vec!["beta".to_string()]);
    assert!(report.resumed.is_empty());

    let restored = windows_of(t, "alpha");
    assert_eq!(restored.len(), alpha_snap.windows.len());

    // The saved *active* window has to end up selected. `create_session` runs a
    // `select-window` for it, and nothing else in this test would notice if that stopped
    // working: every other assertion is about shape, which a wrong selection preserves.
    let saved_active = alpha_snap
        .windows
        .iter()
        .position(|w| w.index == alpha_snap.active_window_index)
        .expect("the snapshot names one of its own windows as active");
    assert_ne!(
        saved_active, 0,
        "precondition: the active window is not the first one, or selecting it would be \
         indistinguishable from doing nothing"
    );
    let active: Vec<bool> = restored.iter().map(|w| w.window_active).collect();
    assert_eq!(
        active.iter().filter(|a| **a).count(),
        1,
        "exactly one window is active: {active:?}"
    );
    assert!(
        restored[saved_active].window_active,
        "the saved active window ('{}') must be the selected one after restore, got {:?}",
        alpha_snap.windows[saved_active].name,
        restored
            .iter()
            .map(|w| (w.window_name.as_str(), w.window_active))
            .collect::<Vec<_>>()
    );

    for (orig, got) in alpha_snap.windows.iter().zip(restored.iter()) {
        assert_eq!(got.window_name, orig.name, "window name mismatch");
        assert_eq!(
            got.window_panes as usize,
            orig.panes.len(),
            "pane count mismatch for window '{}'",
            orig.name
        );
        assert_eq!(
            normalize_layout(&got.window_layout),
            normalize_layout(&orig.layout),
            "window_layout shape mismatch for window '{}' (raw: got {:?}, want {:?})",
            orig.name,
            got.window_layout,
            orig.layout,
        );
    }
}

#[test]
fn restore_uses_window_zeros_first_pane_cwd_not_the_stale_session_path() {
    let s = Server::start();
    let t = &s.0;

    // "alpha" is the server's only session — keep it that way for the `list-windows -a`/
    // `Tmux::list_sessions` reason documented on the resume test below.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();

    // `#{session_path}` is frozen at session-creation time; a pane's own
    // `#{pane_current_path}` tracks its *live* cwd. `cd` inside window 0's only pane so the
    // two diverge, matching the bug this test guards: window 0 restoring at the session's
    // stale creation-time path instead of its own first pane's actual (saved) cwd.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap(); // resolve symlinks (e.g. macOS's
    // /tmp -> /private/tmp) up front, so the string this test expects back after restore
    // matches byte-for-byte what tmux's own OS-level cwd lookup reports, with nothing left
    // for either side to resolve differently.
    let sub = base.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let sub_str = sub.to_str().unwrap();

    t.run_ok(&[
        "send-keys",
        "-t",
        "alpha",
        &format!("cd {sub_str}"),
        "Enter",
    ])
    .unwrap();
    let moved = poll_until(std::time::Duration::from_secs(5), || {
        t.list_panes()
            .unwrap()
            .into_iter()
            .find(|p| p.session_name == "alpha")
            .map(|p| p.pane_current_path == sub_str)
            .unwrap_or(false)
    });
    if !moved {
        let actual = t
            .list_panes()
            .unwrap()
            .into_iter()
            .find(|p| p.session_name == "alpha")
            .map(|p| p.pane_current_path);
        let captured = t.capture_pane("alpha", false);
        panic!(
            "pane never reported having cd'd into {sub_str:?}; actual pane_current_path = {actual:?}; capture-pane = {captured:?}"
        );
    }

    let paths_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(paths_dir.path());

    let snap = snapshot::take(t).unwrap();
    let alpha_snap = snap.sessions.iter().find(|s| s.name == "alpha").unwrap();
    assert_eq!(
        alpha_snap.windows[0].panes[0].cwd, sub_str,
        "snapshot must capture the pane's live cwd, not the stale session path"
    );
    // The session-level path is deliberately *not* what the pane cd'd into — this is what
    // makes the bug reproducible: falling back to it would restore window 0 here instead.
    assert_ne!(alpha_snap.path, sub_str);
    snapshot::save(&paths, "default", &snap).unwrap();

    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let cfg = Config::default();
    let report = restore::apply(t, &cfg, &paths, "default", false).unwrap();
    assert_eq!(report.created, vec!["alpha".to_string()]);

    let restored_cwd = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .unwrap()
        .pane_current_path;
    assert_eq!(
        restored_cwd, sub_str,
        "window 0's restored pane must start at its own saved cwd, not the session's stale path"
    );
}

/// Every pane's saved cwd must land back in *its own* pane after restore, even when the
/// window's layout order differs from a naive creation order. Restore recreates panes with
/// repeated `split-window` and then applies the saved `#{window_layout}` with a single
/// `select-layout`; because tmux assigns panes to layout cells positionally (in pane-list
/// order, ignoring the ids baked into the layout string), splitting the *active* pane each
/// time scrambled the pane list and dealt cwds — and any resumed agent — into the wrong
/// panes. A pane is killed before the snapshot so pane-id numbers diverge from pane_index,
/// guarding against a fix that leans on the two happening to agree.
#[test]
fn restore_keeps_each_pane_cwd_with_its_own_pane_across_a_divergent_layout() {
    let s = Server::start();
    let t = &s.0;

    // Keep the server non-empty after "alpha" is killed, so `list-windows -a` (which
    // `restore::apply` calls) has a current target — same reason as the sibling tests.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let dstr: Vec<String> = (0..5)
        .map(|i| {
            let d = work.path().join(format!("d{i}"));
            std::fs::create_dir_all(&d).unwrap();
            d.to_string_lossy().into_owned()
        })
        .collect();

    // A fresh window whose first pane is in d0, then four more panes in d1..d4 split in a
    // mix of directions, so the resulting layout's spatial order is non-trivial.
    t.run_ok(&["new-window", "-t", "alpha", "-n", "grid", "-c", &dstr[0]])
        .unwrap();
    let first = t.display("alpha:grid", "#{pane_id}").unwrap();
    t.run_ok(&["split-window", "-h", "-t", &first, "-c", &dstr[1]])
        .unwrap();
    t.run_ok(&["split-window", "-v", "-t", &first, "-c", &dstr[2]])
        .unwrap();
    let doomed = t
        .run(&[
            "split-window",
            "-v",
            "-t",
            &first,
            "-c",
            &dstr[3],
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .unwrap();
    t.run_ok(&["split-window", "-h", "-t", "alpha:grid", "-c", &dstr[4]])
        .unwrap();
    // Kill one pane so pane-id numbers no longer track pane_index.
    t.run_ok(&["kill-pane", "-t", doomed.trim()]).unwrap();

    let paths_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(paths_dir.path());
    let snap = snapshot::take(t).unwrap();
    let grid = snap
        .sessions
        .iter()
        .find(|s| s.name == "alpha")
        .unwrap()
        .windows
        .iter()
        .find(|w| w.name == "grid")
        .unwrap();
    let saved_cwds: Vec<String> = grid.panes.iter().map(|p| p.cwd.clone()).collect();
    assert!(
        saved_cwds.len() >= 4,
        "expected a multi-pane window, got {saved_cwds:?}"
    );
    let mut distinct = saved_cwds.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        saved_cwds.len(),
        "the saved cwds must be distinct or a permutation could hide: {saved_cwds:?}"
    );

    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let cfg = Config::default();
    let report = restore::apply(t, &cfg, &paths, "default", false).unwrap();
    assert_eq!(report.created, vec!["alpha".to_string()]);

    let mut restored: Vec<taw::tmux::PaneInfo> = t
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.session_name == "alpha" && p.window_name == "grid")
        .collect();
    restored.sort_by_key(|p| p.pane_index);
    let restored_cwds: Vec<String> = restored
        .iter()
        .map(|p| p.pane_current_path.clone())
        .collect();

    assert_eq!(
        restored_cwds, saved_cwds,
        "each restored pane must sit at the cwd saved for its own spatial slot"
    );
}

#[test]
fn resume_sends_the_agents_resume_command_into_the_restored_pane() {
    let s = Server::start();
    let t = &s.0;

    // This test kills "alpha" outright so restore has to recreate it. `exit-empty off`
    // (set in `Server::start`) keeps the *server process* alive when its last session
    // goes, but a handful of read commands this test and `restore::apply` both depend on
    // (`list-windows -a`, and so `Tmux::list_sessions`) fail with tmux's own "no current
    // target" once the server has literally zero sessions — they need *some* session to
    // resolve against, `-a` or not. A second, never-killed session keeps the server from
    // ever going fully empty; it is otherwise irrelevant to this test (it saves as a
    // second, untouched snapshot entry restore reports as `skipped`, not `created`).
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();

    // A fake `claude` that appends its argv to a file, so the assertion below proves real
    // *execution* (not just that `send-keys` typed the right text — dropping the `Enter`
    // restore.rs sends would still leave the text sitting on the prompt line, unexecuted).
    //
    // Getting a freshly-created pane's shell to actually resolve `claude` to *this* script
    // took real trial and error: neither `tmux set-environment -g PATH ...` nor `-e
    // PATH=...` passed directly on `new-session` reliably reaches a freshly spawned pane's
    // process on every host — confirmed empirically on this one, where a fresh pane's
    // shell `PATH` matched neither, regardless of `default-shell`/`default-command`
    // (avoiding a login shell — and so e.g. macOS's `path_helper` rebuilding `PATH` — ruled
    // that specific culprit out, but the override still never arrived). So this sets `PATH` from
    // *inside* the shell itself instead of asking tmux to pass it in: `default-command` runs
    // `export PATH=...; exec /bin/sh`: `export` sets it in the *current* process, and `exec`
    // replaces the program but keeps that process's (now-exported) environment, depending only on
    // ordinary POSIX shell semantics, not on tmux's environment-passing, which is what made this
    // reliable across repeated runs where the tmux-side mechanisms were not.
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let argv_file = tmp.path().join("claude-argv.txt");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
        argv_file.display()
    );
    let script_path = bin_dir.join("claude");
    std::fs::write(&script_path, script).unwrap();
    crate::common::make_executable(&script_path);

    t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
        .unwrap();
    t.run_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!("export PATH={}:$PATH; exec /bin/sh", bin_dir.display()),
    ])
    .unwrap();

    let pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .unwrap();
    t.set_pane_option(&pane.pane_id, "@agent_kind", "claude")
        .unwrap();
    t.set_pane_option(&pane.pane_id, "@agent_session_id", "abc")
        .unwrap();
    t.set_pane_option(&pane.pane_id, "@agent_name", "reviewer")
        .unwrap();

    let paths_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(paths_dir.path());

    let snap = snapshot::take(t).unwrap();
    snapshot::save(&paths, "default", &snap).unwrap();

    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let cfg = Config::default();
    assert!(cfg.session.resume_agents_on_restore);

    let report = restore::apply(t, &cfg, &paths, "default", false).unwrap();
    assert_eq!(report.resumed.len(), 1, "report: {report:#?}");

    let appeared = poll_until(std::time::Duration::from_secs(5), || {
        std::fs::read_to_string(&argv_file)
            .map(|c| c.contains("--resume") && c.contains("abc"))
            .unwrap_or(false)
    });
    assert!(
        appeared,
        "fake claude was never invoked with --resume abc (argv file: {:?})",
        std::fs::read_to_string(&argv_file)
    );

    // The restored pane must carry the kind hint (and saved name) so re-detection lands on
    // the right identity even before the resumed `claude` process re-reports itself.
    let restored_pane_id = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .unwrap()
        .pane_id;
    assert_eq!(
        t.get_pane_option(&restored_pane_id, "@agent_kind_hint")
            .unwrap(),
        Some("claude".to_string())
    );
    assert_eq!(
        t.get_pane_option(&restored_pane_id, "@agent_name").unwrap(),
        Some("reviewer".to_string())
    );
    // The pane already carries the id it was just resumed from — an autosave that runs
    // before the resumed `claude` process re-reports itself, or a server restart before
    // it ever does, must still resume the same conversation rather than a plain shell.
    assert_eq!(
        t.get_pane_option(&restored_pane_id, "@agent_session_id")
            .unwrap(),
        Some("abc".to_string())
    );
}

/// `[session] startup_per_agent_delay_ms` (herdr v0.9.2, `46a5091838` (#4487, #4102 in v0.9.2's changelog)): restoring
/// two sessions that each resume an agent must not launch both in the same instant.
///
/// Each fake agent only marks *when* it started (creating its own marker file); the test
/// reads the two files' mtimes after `restore::apply` returns rather than polling while it
/// runs, since `apply` is a single blocking call that itself only returns once every
/// configured delay has elapsed.
#[test]
fn restored_agents_start_startup_per_agent_delay_ms_apart() {
    let s = Server::start();
    let t = &s.0;

    // Keeps the server from going fully empty once "alpha" and "beta" are both killed
    // below; see the resume test above for why that matters.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    // A second resumable session, alongside "alpha".
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();

    // Two fake agents — see the resume test above for why `default-command` (rather than
    // tmux handing `PATH` to the pane directly) is what reliably resolves them.
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let claude_marker = tmp.path().join("claude-started");
    let codex_marker = tmp.path().join("codex-started");
    for (name, marker) in [("claude", &claude_marker), ("codex", &codex_marker)] {
        let script = format!("#!/bin/sh\n: > '{}'\n", marker.display());
        let path = bin_dir.join(name);
        std::fs::write(&path, script).unwrap();
        crate::common::make_executable(&path);
        // Run each once now, and forget that it did. The first execution of a script that
        // was written a moment ago is slow on macOS (the security check on a new
        // executable), by an amount that varies from run to run — 400ms and more under
        // load — and that delay, not the shell, was what this test was measuring.
        assert!(
            std::process::Command::new(&path)
                .status()
                .unwrap()
                .success()
        );
        std::fs::remove_file(marker).unwrap();
    }

    t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
        .unwrap();
    t.run_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!("export PATH={}:$PATH; exec /bin/sh", bin_dir.display()),
    ])
    .unwrap();

    let panes = t.list_panes().unwrap();
    let alpha_pane = panes
        .iter()
        .find(|p| p.session_name == "alpha")
        .unwrap()
        .pane_id
        .clone();
    t.set_pane_option(&alpha_pane, "@agent_kind", "claude")
        .unwrap();
    t.set_pane_option(&alpha_pane, "@agent_session_id", "abc")
        .unwrap();
    let beta_pane = panes
        .iter()
        .find(|p| p.session_name == "beta")
        .unwrap()
        .pane_id
        .clone();
    t.set_pane_option(&beta_pane, "@agent_kind", "codex")
        .unwrap();
    t.set_pane_option(&beta_pane, "@agent_session_id", "xyz")
        .unwrap();

    let paths_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(paths_dir.path());
    let snap = snapshot::take(t).unwrap();
    snapshot::save(&paths, "default", &snap).unwrap();

    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
    t.run_ok(&["kill-session", "-t", "beta"]).unwrap();

    let mut cfg = Config::default();
    // Generous next to the noise that is left once the fake agents are warm (a shell
    // starting a few milliseconds late), while still short enough to keep the test fast.
    cfg.session.startup_per_agent_delay_ms = 500;

    let began = std::time::Instant::now();
    let report = restore::apply(t, &cfg, &paths, "default", false).unwrap();
    let took = began.elapsed();
    assert_eq!(report.resumed.len(), 2, "report: {report:#?}");
    // The delay itself, which no shell has a say in: one restore that resumes two agents
    // waits it out once, between them.
    assert!(
        took >= std::time::Duration::from_millis(500),
        "restore returned after {took:?}, before the configured 500ms had passed between resumes"
    );

    // `send-keys` only injects the keystrokes; `apply` returns once it has queued both
    // (with the delay between them already elapsed), not once each pane's shell has
    // actually run the command it was sent.
    let both_appeared = poll_until(std::time::Duration::from_secs(5), || {
        claude_marker.exists() && codex_marker.exists()
    });
    assert!(
        both_appeared,
        "fake claude ({}) or codex ({}) was never invoked",
        claude_marker.exists(),
        codex_marker.exists()
    );

    let claude_started = std::fs::metadata(&claude_marker)
        .unwrap()
        .modified()
        .unwrap();
    let codex_started = std::fs::metadata(&codex_marker)
        .unwrap()
        .modified()
        .unwrap();
    let (earlier, later) = if claude_started <= codex_started {
        (claude_started, codex_started)
    } else {
        (codex_started, claude_started)
    };
    let gap = later.duration_since(earlier).unwrap();
    assert!(
        gap >= std::time::Duration::from_millis(250),
        "resumed agents started only {gap:?} apart, expected close to the configured 500ms"
    );
}

#[test]
fn autosave_if_due_saves_when_the_marker_is_set_and_no_snapshot_exists_yet() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());

    taw::session::autosave_touch(&paths).unwrap();
    let did = taw::session::autosave_if_due(
        t,
        &Config::default(),
        &paths,
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    assert!(did);

    let snap = snapshot::load(&paths, "default").unwrap();
    assert_eq!(snap.sessions.len(), 1);
    assert!(!paths.sessions_dir.join(".autosave-requested").exists());
}

#[test]
fn autosave_if_due_saves_when_the_marker_is_set_and_the_snapshot_is_stale() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());

    let old = taw::session::snapshot::Snapshot {
        version: 1,
        taken_unix: 0,
        sessions: vec![],
        skipped_panes: 0,
    };
    snapshot::save(&paths, "default", &old).unwrap();
    taw::session::autosave_touch(&paths).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));

    let did = taw::session::autosave_if_due(
        t,
        &Config::default(),
        &paths,
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    assert!(did);
    let snap = snapshot::load(&paths, "default").unwrap();
    assert_eq!(snap.sessions.len(), 1, "must reflect the live server now");
}

/// An autosave never writes an empty layout. A shutdown that closes sessions one by
/// one can find the live server briefly empty; overwriting the last good snapshot with
/// that would lose the whole layout for a state nothing actually asked to save.
#[test]
fn autosave_if_due_never_overwrites_a_saved_layout_with_an_empty_one() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());

    taw::session::autosave_touch(&paths).unwrap();
    assert!(
        taw::session::autosave_if_due(t, &Config::default(), &paths, std::time::Duration::ZERO)
            .unwrap()
    );
    let saved = snapshot::load(&paths, "default").unwrap();
    assert_eq!(saved.sessions.len(), 1);

    // The live server now has nothing left, as it briefly would mid-shutdown. A bare
    // `list-sessions` (unlike `Tmux::list_sessions`, which also runs `list-windows -a`)
    // still exits 0 with nothing to show for it.
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
    assert_eq!(t.run(&["list-sessions"]).unwrap().trim(), "");

    taw::session::autosave_touch(&paths).unwrap();
    let did =
        taw::session::autosave_if_due(t, &Config::default(), &paths, std::time::Duration::ZERO)
            .unwrap();
    assert!(!did, "an empty live layout must never be saved");
    let still_saved = snapshot::load(&paths, "default").unwrap();
    assert_eq!(
        still_saved.sessions.len(),
        1,
        "the last good layout must survive untouched"
    );
}

/// A close-triggered autosave skips once tmux itself has gone — the rest of a
/// shutdown that closed one session after another, each firing its own autosave hook, so
/// none of them races to save a partial layout before the whole server is down.
#[test]
fn autosave_after_close_skips_once_tmux_has_gone() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    // A healthy layout already on disk — must survive untouched.
    snapshot::save(&paths, "default", &taw::session::snapshot::take(t).unwrap()).unwrap();

    taw::session::autosave_touch(&paths).unwrap();
    t.run_ok(&["kill-server"]).unwrap();

    let did = taw::session::autosave_after_close(
        t,
        &Config::default(),
        &paths,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    assert!(!did, "tmux is gone; nothing must be saved");
    let saved = snapshot::load(&paths, "default").unwrap();
    assert_eq!(saved.sessions.len(), 1, "the prior save must be untouched");
}

/// The same call, but tmux is still very much alive: it must behave exactly like
/// `autosave_if_due` once its wait has elapsed, not skip just because it waited at all.
#[test]
fn autosave_after_close_still_saves_when_tmux_is_alive() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());

    taw::session::autosave_touch(&paths).unwrap();
    let did = taw::session::autosave_after_close(
        t,
        &Config::default(),
        &paths,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    assert!(did);
    let saved = snapshot::load(&paths, "default").unwrap();
    assert_eq!(saved.sessions.len(), 1);
}

#[test]
fn resume_command_maps_known_agent_kinds_and_none_for_the_rest() {
    assert_eq!(
        restore::resume_command("claude", "abc"),
        Some("claude --resume abc".to_string())
    );
    assert_eq!(
        restore::resume_command("codex", "abc"),
        Some("codex resume abc".to_string())
    );
    assert_eq!(
        restore::resume_command("opencode", "abc"),
        Some("opencode --session abc".to_string())
    );
    assert_eq!(restore::resume_command("gemini", "abc"), None);
    assert_eq!(restore::resume_command("", "abc"), None);
}

#[test]
fn restore_rebuilds_duplicate_and_dotted_window_names_by_addressing_ids() {
    // tmux window names are not unique and are not a target grammar. `<session>:<name>`
    // resolves to the *first* window of that name — so two windows called "dup" would
    // pile every later split onto the first one — and a name holding a `.` is read as
    // `<window>.<pane>`, which fails outright with "can't find pane: dot". Either way
    // restore aborted half-built, leaving a session that matched nothing that was saved.
    let s = Server::start();
    let t = &s.0;

    // Built by window *id*, exactly because the names below cannot address a window.
    let first = t
        .run(&["display-message", "-p", "-t", "alpha:", "#{window_id}"])
        .unwrap();
    t.run_ok(&["rename-window", "-t", &first, "dup"]).unwrap();
    for name in ["dup", "with.dot"] {
        t.run_ok(&["new-window", "-d", "-t", "alpha:", "-n", name])
            .unwrap();
    }
    for window in windows_of(t, "alpha") {
        t.run_ok(&["split-window", "-d", "-t", &window.window_id])
            .unwrap();
    }

    // A second session so the server still has one after "alpha" is killed: with no
    // sessions at all, `list-windows -a` answers "no current target" and every read in
    // this test would fail for a reason that has nothing to do with restore.
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let snap = snapshot::take(t).unwrap();
    let alpha_snap = snap
        .sessions
        .iter()
        .find(|sess| sess.name == "alpha")
        .unwrap()
        .clone();
    assert_eq!(alpha_snap.windows.len(), 3, "precondition: three windows");
    assert_eq!(
        alpha_snap
            .windows
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>(),
        ["dup", "dup", "with.dot"],
        "precondition: two windows share a name and one carries a dot"
    );
    for window in &alpha_snap.windows {
        assert_eq!(window.panes.len(), 2, "precondition: two panes per window");
    }

    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();
    assert_eq!(report.created, vec!["alpha".to_string()]);

    let restored = windows_of(t, "alpha");
    assert_eq!(
        restored
            .iter()
            .map(|w| w.window_name.as_str())
            .collect::<Vec<_>>(),
        ["dup", "dup", "with.dot"],
        "every saved window must come back, under its own name"
    );
    for (orig, got) in alpha_snap.windows.iter().zip(restored.iter()) {
        assert_eq!(
            got.window_panes, 2,
            "window '{}' must hold its own two panes, not another window's",
            orig.name
        );
        assert_eq!(
            normalize_layout(&got.window_layout),
            normalize_layout(&orig.layout),
            "window_layout shape mismatch for '{}'",
            orig.name
        );
    }
}

#[test]
fn restore_refuses_to_resume_a_saved_session_id_that_would_reach_the_shell() {
    // `taw pane report-agent-session` rejects such an id at the write, but a snapshot
    // on disk can predate that check — and it is read back into a command delivered with
    // `send-keys … Enter`, which the pane's *shell* runs. Restore must build the pane and
    // leave it a plain shell, saying in the report why it did.
    let s = Server::start();
    let t = &s.0;

    // Keeps the server from ever holding zero sessions; see the resume test above.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();

    let pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .unwrap();
    t.set_pane_option(&pane.pane_id, "@agent_kind", "claude")
        .unwrap();
    // Written straight to tmux, the way a snapshot from an older build would have.
    t.set_pane_option(&pane.pane_id, "@agent_session_id", "abc; touch /tmp/pwned")
        .unwrap();

    let paths_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(paths_dir.path());
    let snap = snapshot::take(t).unwrap();
    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let cfg = Config::default();
    assert!(cfg.session.resume_agents_on_restore);
    let report = restore::apply(t, &cfg, &paths, "default", false).unwrap();

    assert_eq!(report.created, vec!["alpha".to_string()]);
    assert!(
        report.resumed.is_empty(),
        "nothing may be resumed from that id: {report:#?}"
    );
    assert!(
        report
            .plan
            .iter()
            .any(|line| line.contains("skip resume") && line.contains("not a valid agent session")),
        "the report must say why the pane was left alone: {:#?}",
        report.plan
    );

    // The pane itself is still restored, just without a resumed agent in it.
    let restored = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha");
    assert!(restored.is_some(), "the session must still be recreated");
    assert_eq!(
        t.get_pane_option(&restored.unwrap().pane_id, "@agent_kind_hint")
            .unwrap(),
        None,
        "no kind hint either: nothing was resumed into this pane"
    );
}

// ---------------------------------------------------------------------------------------
// Typing into a shell that is not at its prompt yet
//
// A pane taw has just made has a shell that is still starting, and a shell can ask a
// question before it gets to its prompt (mise asks "Trust them? Yes No All" when a folder's
// config is new to it). What is typed then goes to whatever reads the terminal first: a
// restore that typed `claude --resume <id>` at once answered that question with an `a`.

/// The shells a restored pane can come up with, and the fake `claude` they resolve.
///
/// What asks is a program of its own that reads the terminal, as mise does, and not the shell:
/// the line it reads is whatever is typed first, and it writes that line down so a test can
/// say who got what.
struct Shells {
    dir: tempfile::TempDir,
}

impl Shells {
    fn new() -> Shells {
        let shells = Shells {
            dir: tempfile::tempdir().unwrap(),
        };
        let write = |name: &str, body: &str| {
            let path = shells.dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            crate::common::make_executable(&path);
        };
        write(
            "claude",
            &format!("printf '%s\\n' \"$@\" > '{}'\n", shells.argv().display()),
        );
        // Run once now, and forget that it did. The first run of a script written a moment ago is
        // slow on macOS (the check on a new executable, a few hundred milliseconds), and that,
        // not the shell, would be what the timing of a resume measured.
        assert!(
            std::process::Command::new(shells.dir.path().join("claude"))
                .status()
                .unwrap()
                .success()
        );
        std::fs::remove_file(shells.argv()).unwrap();
        // A `pi` as well, for the panes that are resumed by their session path: the one a machine
        // may have must not be started by a test.
        write(
            "pi",
            &format!("printf '%s\\n' \"$@\" > '{}'\n", shells.pi_argv().display()),
        );
        write(
            "ask",
            &format!(
                "printf 'Trust them? Yes No All '\nread answer\nprintf '%s' \"$answer\" > '{}'\n",
                shells.answer().display()
            ),
        );
        shells
    }

    /// What the fake `claude` was run with, once it has been.
    fn argv(&self) -> std::path::PathBuf {
        self.dir.path().join("claude-argv.txt")
    }

    /// What the fake `pi` was run with, once it has been.
    fn pi_argv(&self) -> std::path::PathBuf {
        self.dir.path().join("pi-argv.txt")
    }

    /// What the question read, once it has been answered.
    fn answer(&self) -> std::path::PathBuf {
        self.dir.path().join("answer.txt")
    }

    /// Made by the shell's own prompt, the first time it draws one: the moment the pane's prompt
    /// can be said to be there.
    fn prompt(&self) -> std::path::PathBuf {
        self.dir.path().join("prompt-reached")
    }

    /// Every pane tmux starts from now on asks its question, and then is a `/bin/sh`.
    fn ask_first(&self, t: &Tmux) {
        self.start_panes_with(t, &format!("'{}'; ", self.dir.path().join("ask").display()));
    }

    /// Every pane tmux starts from now on is a real zsh, as a user's is, whose startup file
    /// runs the question before its first prompt (mise asks from a hook zsh runs before it draws
    /// one). zsh gives a program it runs a process group of its own and the terminal, which a
    /// `sh -c` does not: the shell has to be told not to be at its prompt either way.
    fn ask_first_in_zsh(&self, t: &Tmux) {
        let zdotdir = self.dir.path().join("zdotdir");
        std::fs::create_dir_all(&zdotdir).unwrap();
        std::fs::write(
            zdotdir.join(".zshrc"),
            format!(
                "export PATH='{}':$PATH\nPS1='READY> '\n'{}'\n",
                self.dir.path().display(),
                self.dir.path().join("ask").display()
            ),
        )
        .unwrap();
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/zsh"])
            .unwrap();
        // The server's own variable, so the pane's zsh reads this directory's startup files and
        // not the ones in the home directory of whoever runs the suite.
        t.run_ok(&[
            "set-environment",
            "-g",
            "ZDOTDIR",
            &zdotdir.display().to_string(),
        ])
        .unwrap();
    }

    /// Every pane tmux starts from now on is a real zsh that works for a while before it asks
    /// its question, and says nothing while it does: its startup file loops in builtins, with no
    /// program run and nothing drawn, for as long as it is told. That is a shell that is idle by
    /// the process table (it is alone in front of the terminal, and a shell) and whose screen
    /// is not moving, with the question it has still to ask coming after.
    fn ask_after_working_in_silence_in_zsh(&self, t: &Tmux, seconds: f64) {
        let zdotdir = self.dir.path().join("zdotdir-silent");
        std::fs::create_dir_all(&zdotdir).unwrap();
        std::fs::write(
            zdotdir.join(".zshrc"),
            format!(
                "zmodload zsh/datetime\n\
                 until=$(( EPOCHREALTIME + {seconds} ))\n\
                 while (( EPOCHREALTIME < until )); do :; done\n\
                 export PATH='{bin}':$PATH\n\
                 PS1='READY> '\n\
                 '{ask}'\n",
                bin = self.dir.path().display(),
                ask = self.dir.path().join("ask").display()
            ),
        )
        .unwrap();
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/zsh"])
            .unwrap();
        t.run_ok(&[
            "set-environment",
            "-g",
            "ZDOTDIR",
            &zdotdir.display().to_string(),
        ])
        .unwrap();
    }

    /// Every pane tmux starts from now on is a `/bin/sh` that notes the moment it draws its first
    /// prompt: the prompt itself makes the file, once, as it is drawn (bash expands a command
    /// substitution in `PS1` each time it draws one).
    fn come_straight_to_the_prompt(&self, t: &Tmux) {
        let marker = self.prompt();
        self.start_panes_with(
            t,
            &format!(
                "export PS1='$([ -e {marker} ] || : > {marker})$ '; ",
                marker = marker.display()
            ),
        );
    }

    /// `default-command`: set from inside the shell, as the resume test above explains.
    fn start_panes_with(&self, t: &Tmux, before_the_prompt: &str) {
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        t.run_ok(&[
            "set-option",
            "-g",
            "default-command",
            &format!(
                "export PATH={}:$PATH; {before_the_prompt}exec /bin/sh",
                self.dir.path().display()
            ),
        ])
        .unwrap();
    }
}

/// Saves a session "alpha" whose one pane held a `claude` conversation, and ends it, so a
/// restore has it to recreate. A second session keeps the server from ever being empty.
fn save_alpha_with_a_claude_pane(t: &Tmux, paths: &Paths) {
    save_alpha_with_a_pane_holding(
        t,
        paths,
        &[("@agent_kind", "claude"), ("@agent_session_id", "abc")],
    );
}

/// Saves a session "alpha" whose one pane had `options` set, and ends it, so a restore has it
/// to recreate. A second session keeps the server from ever being empty.
fn save_alpha_with_a_pane_holding(t: &Tmux, paths: &Paths, options: &[(&str, &str)]) {
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    let pane = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .unwrap();
    for (name, value) in options {
        t.set_pane_option(&pane.pane_id, name, value).unwrap();
    }
    snapshot::save(paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
}

/// The restored "alpha"'s pane, once there is one.
fn alpha_pane(t: &Tmux) -> Option<String> {
    t.list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .map(|p| p.pane_id)
}

/// A restore run on its own thread, for a test that has to watch the pane while it waits.
fn restore_in_the_background(
    t: &Tmux,
    dir: &std::path::Path,
    wait: taw::session::prompt::Timing,
) -> std::thread::JoinHandle<anyhow::Result<restore::Report>> {
    let t = t.clone();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        restore::apply_with(
            &t,
            &Config::default(),
            &test_paths(&dir),
            "default",
            false,
            &wait,
        )
    })
}

/// The same tmux server reached through a `tmux` that ends a pane the first time it is asked to
/// run `before`, just before it runs: the pane that command names, or the active pane of the
/// window it names. A file it makes says it did, so a pass is not a run that never lost one.
fn tmux_that_loses_a_pane_before(
    t: &Tmux,
    dir: &std::path::Path,
    before: &str,
) -> (Tmux, std::path::PathBuf) {
    let socket = match &t.socket {
        Some(taw::tmux::Socket::Name(name)) => name.clone(),
        other => panic!("a named socket is what these tests start: {other:?}"),
    };
    let lost = dir.join("lost-a-pane");
    let script = dir.join("tmux-that-loses-a-pane");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             target=''\n\
             prev=''\n\
             for arg in \"$@\"; do\n\
             \x20 [ \"$prev\" = -t ] && target=\"$arg\"\n\
             \x20 prev=\"$arg\"\n\
             done\n\
             for arg in \"$@\"; do\n\
             \x20 if [ \"$arg\" = '{before}' ] && [ -n \"$target\" ] && [ ! -e '{lost}' ]; then\n\
             \x20   : > '{lost}'\n\
             \x20   '{real}' -L '{socket}' kill-pane -t \"$target\" > /dev/null 2>&1\n\
             \x20 fi\n\
             done\n\
             exec '{real}' \"$@\"\n",
            lost = lost.display(),
            real = t.bin.display(),
        ),
    )
    .unwrap();
    crate::common::make_executable(&script);
    (
        Tmux {
            bin: script,
            socket: t.socket.clone(),
        },
        lost,
    )
}

/// The same tmux server reached through a `tmux` that writes down every command it is given,
/// each on a line, before it runs it: how a test knows what a restore running on another thread
/// has got to, which nothing else shows from outside.
fn tmux_that_writes_down_its_commands(
    t: &Tmux,
    dir: &std::path::Path,
) -> (Tmux, std::path::PathBuf) {
    let log = dir.join("tmux-commands");
    let script = dir.join("tmux-that-writes-down-its-commands");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\nexec '{real}' \"$@\"\n",
            log = log.display(),
            real = t.bin.display(),
        ),
    )
    .unwrap();
    crate::common::make_executable(&script);
    (
        Tmux {
            bin: script,
            socket: t.socket.clone(),
        },
        log,
    )
}

#[test]
fn restore_types_nothing_while_the_shell_is_asking_a_question_and_the_command_once_it_is_answered()
{
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.ask_first(t);
    restore_waits_for_the_question_to_be_answered(t, &shells, paths_dir.path());
}

/// The same as above in a real zsh. Skipped where there is none: the `/bin/sh` panes of the
/// other tests are what every machine has.
#[test]
fn restore_types_nothing_while_a_zsh_is_asking_a_question_and_the_command_once_it_is_answered() {
    if !std::path::Path::new("/bin/zsh").exists() {
        return;
    }
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.ask_first_in_zsh(t);
    restore_waits_for_the_question_to_be_answered(t, &shells, paths_dir.path());
}

fn restore_waits_for_the_question_to_be_answered(
    t: &Tmux,
    shells: &Shells,
    paths_dir: &std::path::Path,
) {
    let restoring = restore_in_the_background(t, paths_dir, Default::default());

    // The restored pane comes up with the question on its screen.
    let mut pane = String::new();
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            match alpha_pane(t) {
                Some(id)
                    if t.capture_pane(&id, false)
                        .is_ok_and(|s| s.contains("Trust them?")) =>
                {
                    pane = id;
                    true
                }
                _ => false,
            }
        }),
        "the restored pane never showed the question"
    );

    // While it is up, nothing is typed: not at once, and not a little later either. Long enough
    // for any pause a restore could take to settle on the shell being ready.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        !shells.answer().exists(),
        "something answered the question before anyone did: {:?}",
        std::fs::read_to_string(shells.answer())
    );
    assert!(
        !shells.argv().exists(),
        "claude ran with the question still up"
    );
    assert!(
        !restoring.is_finished(),
        "restore was done while the shell was still asking: it typed into the question"
    );
    assert_eq!(
        t.get_pane_option(&pane, "@agent_kind_hint").unwrap(),
        None,
        "the pane was marked as an agent's before its command had been typed"
    );

    // Answered, the shell goes on to its prompt, and the command is typed there.
    t.run_ok(&["send-keys", "-t", &pane, "y", "Enter"]).unwrap();
    let report = restoring.join().unwrap().unwrap();
    assert_eq!(report.resumed, vec![pane.clone()], "report: {report:#?}");
    assert!(
        poll_until(std::time::Duration::from_secs(15), || {
            std::fs::read_to_string(shells.argv())
                .is_ok_and(|c| c.contains("--resume") && c.contains("abc"))
        }),
        "claude was never run with --resume abc after the question was answered (argv: {:?})",
        std::fs::read_to_string(shells.argv())
    );
    assert_eq!(
        std::fs::read_to_string(shells.answer()).unwrap(),
        "y",
        "the question must have read what was typed to answer it, and only that"
    );
    assert_eq!(
        t.get_pane_option(&pane, "@agent_kind_hint").unwrap(),
        Some("claude".to_string())
    );
    // Marked as holding the agent as soon as its command is typed, as `agent start` does the
    // moment it has identified the agent: a snapshot taken from here on keeps it.
    assert_eq!(
        t.get_pane_option(&pane, "@agent_kind").unwrap(),
        Some("claude".to_string()),
        "the pane was not marked as holding the agent once its command was typed"
    );
}

#[test]
fn restore_types_into_a_shell_that_comes_straight_to_its_prompt_within_a_second_of_it() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.come_straight_to_the_prompt(t);

    // What one look at a pane costs on this machine just now (a command to tmux, and `ps` and
    // `pgrep` over the process table): the wait is made of them, a few to the settle, so how
    // long it takes to get to typing is counted in these.
    let probe = t.list_panes().unwrap().remove(0).pane_id;
    let began = std::time::Instant::now();
    taw::session::prompt::Waiter::new(&probe, began, Default::default())
        .look(t)
        .unwrap();
    let one_look = began.elapsed();

    let report = restore::apply(
        t,
        &Config::default(),
        &test_paths(paths_dir.path()),
        "default",
        false,
    )
    .unwrap();
    assert_eq!(report.resumed.len(), 1, "report: {report:#?}");

    assert!(
        poll_until(std::time::Duration::from_secs(30), || shells
            .argv()
            .exists()),
        "claude was never run"
    );
    let mtime = |path: std::path::PathBuf| std::fs::metadata(path).unwrap().modified().unwrap();
    let after_the_prompt = mtime(shells.argv())
        .duration_since(mtime(shells.prompt()))
        .expect("claude ran before the shell had come to its prompt");
    // Not at once: a shell that has only just come up is waited on for the settle.
    assert!(
        after_the_prompt >= std::time::Duration::from_millis(250),
        "the command ran {after_the_prompt:?} after the shell came to its prompt, with its screen not yet still"
    );
    // And not much later: about half a second on a machine that is not busy, and a bound that
    // tight fails now and then on one that is, so this one grows with what a look costs just
    // then. It is for a restore that sat out its 15 seconds, or looked twice at everything.
    let allowed = std::time::Duration::from_millis(1500) + 20 * one_look;
    assert!(
        after_the_prompt < allowed,
        "the command ran {after_the_prompt:?} after the shell came to its prompt, \
         where a look at a pane takes {one_look:?} and {allowed:?} is allowed"
    );
}

#[test]
fn restore_types_nothing_when_the_shell_never_comes_to_its_prompt_and_says_what_to_run() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.ask_first(t);

    // Nobody answers. A short bound stands in for the 15 seconds a real restore gives a shell.
    let began = std::time::Instant::now();
    let report = restore::apply_with(
        t,
        &Config::default(),
        &test_paths(paths_dir.path()),
        "default",
        false,
        &taw::session::prompt::Timing {
            bound: std::time::Duration::from_secs(2),
            ..Default::default()
        },
    )
    .unwrap();
    let took = began.elapsed();
    assert!(
        took >= std::time::Duration::from_secs(2),
        "restore gave up after {took:?}, before the bound"
    );
    // The bound it was given, and not the 15 seconds a restore gives otherwise: six times what
    // this one was asked to wait is room for a busy machine and not for the longer wait.
    assert!(
        took < std::time::Duration::from_secs(12),
        "restore took {took:?}"
    );

    let pane = alpha_pane(t).expect("the session is restored all the same");
    assert_eq!(report.created, vec!["alpha".to_string()]);
    assert!(
        report.resumed.is_empty(),
        "a pane that never came to its prompt was resumed: {report:#?}"
    );
    assert_eq!(
        report.not_resumed,
        vec![pane.clone()],
        "the result must name the pane nothing was typed into: {report:#?}"
    );
    assert!(
        report.plan.iter().any(|line| line
            == &format!(
                "pane {pane} did not reach its prompt; resume it with: claude --resume abc"
            )),
        "the plan must say, in plain words, what was not done and what to run: {:#?}",
        report.plan
    );
    assert!(
        !shells.answer().exists(),
        "something was typed into the question: {:?}",
        std::fs::read_to_string(shells.answer())
    );
    assert!(!shells.argv().exists());
    assert_eq!(
        t.get_pane_option(&pane, "@agent_kind_hint").unwrap(),
        None,
        "a pane nothing was typed into is not marked as holding an agent"
    );
}

#[test]
fn restore_leaves_a_pane_that_is_closed_while_it_waits_for_its_prompt_and_says_so() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.ask_first(t);

    let (watched, commands) = tmux_that_writes_down_its_commands(t, paths_dir.path());
    let restoring = restore_in_the_background(&watched, paths_dir.path(), Default::default());
    let mut pane = String::new();
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            match alpha_pane(t) {
                Some(id)
                    if t.capture_pane(&id, false)
                        .is_ok_and(|s| s.contains("Trust them?")) =>
                {
                    pane = id;
                    true
                }
                _ => false,
            }
        }),
        "the restored pane never showed the question"
    );
    // Closed once the restore is looking at it, and not while the session is still being made.
    assert!(
        poll_until(std::time::Duration::from_secs(30), || {
            std::fs::read_to_string(&commands).is_ok_and(|log| {
                log.lines().any(|line| {
                    line.contains("display-message") && line.contains(&format!("-t {pane} "))
                })
            })
        }),
        "the restore never looked at the pane"
    );
    t.run_ok(&["kill-pane", "-t", &pane]).unwrap();
    let report = restoring.join().unwrap().unwrap();

    assert!(report.resumed.is_empty(), "report: {report:#?}");
    assert_eq!(
        report.not_resumed,
        vec![pane.clone()],
        "report: {report:#?}"
    );
    assert!(
        report.plan.iter().any(|line| line
            == &format!(
                "pane {pane} was closed before it reached its prompt; resume it with: claude --resume abc"
            )),
        "the plan must say what happened to the pane and what to run: {:#?}",
        report.plan
    );
    assert!(!shells.argv().exists());
}

#[test]
fn restore_leaves_a_pane_that_closes_just_as_its_command_is_typed_and_says_so() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.come_straight_to_the_prompt(t);

    // The shell is at its prompt when it is looked at, and the pane is gone when the command
    // is typed (the `if-shell` that sends it only while the shell is in front): nothing is left to
    // type into, and the restore does not stop at it.
    let (lossy, lost) = tmux_that_loses_a_pane_before(t, paths_dir.path(), "if-shell");
    let report = restore::apply(
        &lossy,
        &Config::default(),
        &test_paths(paths_dir.path()),
        "default",
        false,
    )
    .expect("a pane that closed under the command is not a reason for the restore to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");

    assert!(report.resumed.is_empty(), "report: {report:#?}");
    assert_eq!(report.not_resumed.len(), 1, "report: {report:#?}");
    let pane = &report.not_resumed[0];
    assert!(
        report.plan.iter().any(|line| line
            == &format!(
                "pane {pane} was closed before it reached its prompt; resume it with: claude --resume abc"
            )),
        "the plan must say what happened to the pane and what to run: {:#?}",
        report.plan
    );
    assert!(!shells.argv().exists());
}

// ---------------------------------------------------------------------------------------
// A pane that ends while the restore is still making its session
//
// A pane's shell can end at any moment (a startup file that exits, a machine too busy to run
// it), and tmux takes the pane away with it. A restore that names a window by the id of that
// window's first pane (`select-window -t %7`) then gets "can't find pane" from tmux. The tests
// below end a pane at the moment a restore is about to name it.

#[test]
fn restore_selects_the_saved_window_when_its_first_pane_ended_before_the_selection() {
    let s = Server::start();
    let t = &s.0;

    // Three windows of two panes each, the second one the window that was in front.
    let first = t
        .run(&["display-message", "-p", "-t", "alpha:", "#{window_id}"])
        .unwrap();
    t.run_ok(&["rename-window", "-t", &first, "dup"]).unwrap();
    for name in ["dup", "with.dot"] {
        t.run_ok(&["new-window", "-d", "-t", "alpha:", "-n", name])
            .unwrap();
    }
    for window in windows_of(t, "alpha") {
        t.run_ok(&["split-window", "-d", "-t", &window.window_id])
            .unwrap();
    }
    t.run_ok(&["select-window", "-t", "alpha:1"]).unwrap();
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let snap = snapshot::take(t).unwrap();
    let saved = snap
        .sessions
        .iter()
        .find(|sess| sess.name == "alpha")
        .unwrap();
    assert_eq!(
        saved.active_window_index, 1,
        "precondition: the second window was in front"
    );
    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    // The first pane of the window about to be selected ends first.
    let (lossy, lost) = tmux_that_loses_a_pane_before(t, tmp.path(), "select-window");
    let report = restore::apply(&lossy, &Config::default(), &paths, "default", false)
        .expect("a pane that ended is not a reason for the restore to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");

    assert_eq!(report.created, vec!["alpha".to_string()], "{report:#?}");
    let restored = windows_of(t, "alpha");
    assert_eq!(
        restored
            .iter()
            .map(|w| w.window_name.as_str())
            .collect::<Vec<_>>(),
        ["dup", "dup", "with.dot"]
    );
    assert_eq!(
        restored.iter().map(|w| w.window_panes).collect::<Vec<_>>(),
        [2, 1, 2],
        "the second window lost the pane that ended, and only that"
    );
    assert_eq!(
        restored.iter().map(|w| w.window_active).collect::<Vec<_>>(),
        [false, true, false],
        "the window that was in front is in front"
    );
    // Two windows are called `dup`, so the number is what says which one lost its pane: the second.
    assert!(
        report.plan.iter().any(|line| line
            == "window 1 'dup' has 1 of its 2 panes: the rest closed while the session was being restored"),
        "the plan must say which window has a pane fewer than was saved: {:#?}",
        report.plan
    );
    assert_eq!(
        report
            .plan
            .iter()
            .filter(|line| line.contains("panes: the rest closed"))
            .count(),
        1,
        "only the window that lost a pane is said to have: {:#?}",
        report.plan
    );
}

#[test]
fn restore_goes_on_to_the_next_session_when_a_pane_ends_under_the_one_it_is_making() {
    let s = Server::start();
    let t = &s.0;
    t.run_ok(&["rename-window", "-t", "alpha:", "lone"])
        .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "alpha:"]).unwrap();
    t.run_ok(&["new-session", "-d", "-s", "gamma", "-x", "200", "-y", "50"])
        .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "gamma:"]).unwrap();
    // Stays up, so the server has a session to answer for when the others are gone.
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
    t.run_ok(&["kill-session", "-t", "gamma"]).unwrap();

    // The only pane alpha has so far ends before the split that would give it a second, and
    // with it the window and the session.
    let (lossy, lost) = tmux_that_loses_a_pane_before(t, tmp.path(), "split-window");
    let report = restore::apply(&lossy, &Config::default(), &paths, "default", false)
        .expect("a session that closed under the restore is not a reason for it to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");

    assert_eq!(report.created, vec!["gamma".to_string()], "{report:#?}");
    assert!(
        report.plan.iter().any(|line| line
            == "stopped restoring session 'alpha': a pane or window of it closed while it was being made; windows made: none; not made: 0 'lone'"),
        "the plan must say the session was given up on, and which of its windows were made: {:#?}",
        report.plan
    );
    assert_eq!(
        windows_of(t, "gamma")[0].window_panes,
        2,
        "the next session is whole"
    );
}

#[test]
fn restore_goes_on_when_the_window_it_is_about_to_select_has_closed_altogether() {
    let s = Server::start();
    let t = &s.0;

    // Two windows of one pane each, the second in front: when it is selected it has no pane left.
    t.run_ok(&["new-window", "-d", "-t", "alpha:", "-n", "front"])
        .unwrap();
    t.run_ok(&["select-window", "-t", "alpha:1"]).unwrap();
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let snap = snapshot::take(t).unwrap();
    assert_eq!(
        snap.sessions
            .iter()
            .find(|sess| sess.name == "alpha")
            .unwrap()
            .active_window_index,
        1,
        "precondition: the second window was in front"
    );
    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let (lossy, lost) = tmux_that_loses_a_pane_before(t, tmp.path(), "select-window");
    let report = restore::apply(&lossy, &Config::default(), &paths, "default", false)
        .expect("a window that closed before it was selected is not a reason to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");

    assert_eq!(report.created, vec!["alpha".to_string()], "{report:#?}");
    for line in [
        "could not select window 1 'front': it closed while the session was being restored",
        "window 1 'front' was closed while the session was being restored",
    ] {
        assert!(
            report.plan.iter().any(|plan| plan == line),
            "the plan must say {line:?}: {:#?}",
            report.plan
        );
    }
    assert_eq!(
        windows_of(t, "alpha").len(),
        1,
        "only the window that did not close is left"
    );
}

// ---------------------------------------------------------------------------------------
// A restore that stops early still types the resumes it has planned
//
// Every command is typed once every session is made, so a restore that ends before then (a
// session tmux will not make, a session that closes under it) would lose the agents of the
// sessions it did make, and say nothing.

/// Saves `alpha` (its one pane holds a `claude` conversation), `gamma` and a `keepalive`, then
/// ends alpha and gamma: what a restore has to recreate, in that order. `gamma_layout` replaces
/// the layout saved for gamma's window, with one tmux cannot read.
fn save_alpha_and_gamma(t: &Tmux, paths: &Paths, gamma_layout: &str) {
    t.run_ok(&["new-session", "-d", "-s", "gamma", "-x", "200", "-y", "50"])
        .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    let pane = alpha_pane(t).unwrap();
    t.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    t.set_pane_option(&pane, "@agent_session_id", "abc")
        .unwrap();
    let mut snap = snapshot::take(t).unwrap();
    let names: Vec<&str> = snap
        .sessions
        .iter()
        .map(|sess| sess.name.as_str())
        .collect();
    assert_eq!(
        &names[..2],
        ["alpha", "gamma"],
        "precondition: alpha is made before gamma"
    );
    snap.sessions
        .iter_mut()
        .find(|sess| sess.name == "gamma")
        .unwrap()
        .windows[0]
        .layout = gamma_layout.to_string();
    snapshot::save(paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
    t.run_ok(&["kill-session", "-t", "gamma"]).unwrap();
}

#[test]
fn restore_types_the_resumes_it_has_planned_when_a_later_session_fails_and_gives_the_error_back() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    shells.come_straight_to_the_prompt(t);
    save_alpha_and_gamma(t, &paths, "not a layout");

    let err = restore::apply(t, &Config::default(), &paths, "default", false)
        .expect_err("a layout tmux cannot read is an error");
    assert!(
        format!("{err:#}").contains("layout"),
        "the error must be the one tmux gave, not another: {err:#}"
    );

    // Alpha was made before gamma failed, and its agent is not lost with the restore.
    assert!(
        poll_until(std::time::Duration::from_secs(15), || {
            std::fs::read_to_string(shells.argv())
                .is_ok_and(|c| c.contains("--resume") && c.contains("abc"))
        }),
        "alpha was made and its command was never typed (argv: {:?})",
        std::fs::read_to_string(shells.argv())
    );
}

#[test]
fn a_restore_that_fails_says_in_its_error_which_panes_it_left_alone_and_what_to_run_in_them() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    save_alpha_and_gamma(t, &paths, "not a layout");
    // Alpha's shell is asking a question that nobody answers, and gamma, after it, fails: the
    // restore gives the error back, and with it nothing else to say that alpha's agent was
    // never started, or how to start it.
    shells.ask_first(t);

    let err = restore::apply_with(
        t,
        &Config::default(),
        &paths,
        "default",
        false,
        &taw::session::prompt::Timing {
            bound: std::time::Duration::from_secs(1),
            ..Default::default()
        },
    )
    .expect_err("a layout tmux cannot read is an error");

    let message = err.to_string();
    let pane = alpha_pane(t).expect("alpha was made");
    let left_alone =
        format!("pane {pane} did not reach its prompt; resume it with: claude --resume abc");
    assert_eq!(
        message.lines().last(),
        Some(left_alone.as_str()),
        "the error does not say what was left alone, and what to run in it:\n{message}"
    );
    // The error is the one tmux gave, first, and the lines come after it.
    assert!(
        message.lines().next().unwrap().contains("layout"),
        "the error must still begin with the one tmux gave:\n{message}"
    );
    assert_eq!(message.lines().count(), 2, "{message}");
}

#[test]
fn restore_hands_back_the_resumes_of_the_windows_it_made_when_a_session_is_cut_short() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    shells.come_straight_to_the_prompt(t);

    // `front` holds the agent; `back` has two panes, so a split is made for it.
    t.run_ok(&["rename-window", "-t", "alpha:", "front"])
        .unwrap();
    t.run_ok(&["new-window", "-d", "-t", "alpha:", "-n", "back"])
        .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "alpha:back"])
        .unwrap();
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();
    let front = t.display("alpha:front", "#{pane_id}").unwrap();
    t.set_pane_option(&front, "@agent_kind", "claude").unwrap();
    t.set_pane_option(&front, "@agent_session_id", "abc")
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    // The only pane `back` has so far ends before the split that would give it a second, and
    // with it the window: the session is cut short after `front`, which was made whole.
    let (lossy, lost) = tmux_that_loses_a_pane_before(t, tmp.path(), "split-window");
    let report = restore::apply(&lossy, &Config::default(), &paths, "default", false)
        .expect("a window that closed under the restore is not a reason for it to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");

    assert!(
        report.created.is_empty(),
        "a session cut short is not created: {report:#?}"
    );
    assert!(
        report.plan.iter().any(|line| line
            == "stopped restoring session 'alpha': a pane or window of it closed while it was being made; windows made: 0 'front'; not made: 1 'back'"),
        "the plan must say which of the session's windows were made: {:#?}",
        report.plan
    );
    // The agent of the window that was made is typed, or listed with its command: here, typed.
    let restored_front = alpha_pane(t).expect("front is still there");
    assert_eq!(report.resumed, vec![restored_front], "report: {report:#?}");
    assert!(
        poll_until(std::time::Duration::from_secs(15), || {
            std::fs::read_to_string(shells.argv())
                .is_ok_and(|c| c.contains("--resume") && c.contains("abc"))
        }),
        "front was made and its command was never typed (argv: {:?})",
        std::fs::read_to_string(shells.argv())
    );
}

#[test]
fn the_plan_names_every_window_by_its_saved_number_and_name_when_names_repeat_and_numbers_skip() {
    let s = Server::start();
    let t = &s.0;

    // Two windows called `zsh`, a window that was closed before the save and left a gap in the
    // numbers, and `nvim` after it: 0 'zsh', 1 'zsh' (two panes, so it is split) and 3 'nvim'.
    t.run_ok(&["rename-window", "-t", "alpha:", "zsh"]).unwrap();
    for name in ["zsh", "closed", "nvim"] {
        t.run_ok(&["new-window", "-d", "-t", "alpha:", "-n", name])
            .unwrap();
    }
    t.run_ok(&["split-window", "-d", "-t", "alpha:1"]).unwrap();
    t.run_ok(&["kill-window", "-t", "alpha:2"]).unwrap();
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let snap = snapshot::take(t).unwrap();
    let saved = snap
        .sessions
        .iter()
        .find(|sess| sess.name == "alpha")
        .unwrap();
    assert_eq!(
        saved.windows.iter().map(|w| w.index).collect::<Vec<_>>(),
        [0, 1, 3],
        "precondition: a gap where the closed window was"
    );
    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    // What a dry run says is to be made.
    let dry = restore::apply(t, &Config::default(), &paths, "default", true).unwrap();
    for line in [
        "  use first window as window 0 'zsh' (1 pane(s), layout ",
        "  create window 1 'zsh' (2 pane(s), layout ",
        "  create window 3 'nvim' (1 pane(s), layout ",
    ] {
        assert!(
            dry.plan.iter().any(|plan| plan.starts_with(line)),
            "the plan must say {line:?}: {:#?}",
            dry.plan
        );
    }
    assert!(
        dry.plan
            .iter()
            .any(|plan| plan == "  select window 0 'zsh'"),
        "{:#?}",
        dry.plan
    );

    // The only pane the second window has so far ends before the split that would give it a
    // second, and with it the window: the session is cut short after the first window, and the
    // third was never begun.
    let (lossy, lost) = tmux_that_loses_a_pane_before(t, tmp.path(), "split-window");
    let report = restore::apply(&lossy, &Config::default(), &paths, "default", false)
        .expect("a window that closed under the restore is not a reason for it to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");
    assert!(
        report.plan.iter().any(|line| line
            == "stopped restoring session 'alpha': a pane or window of it closed while it was being made; windows made: 0 'zsh'; not made: 1 'zsh', 3 'nvim'"),
        "the plan must say which windows were made and which not, by the numbers they had: {:#?}",
        report.plan
    );
}

#[test]
fn a_session_whose_every_window_closed_is_not_created_and_the_plan_says_it_was_given_up_on() {
    let s = Server::start();
    let t = &s.0;
    t.run_ok(&["rename-window", "-t", "alpha:", "only"])
        .unwrap();
    t.run_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"])
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    // Its one window, and with it the session, is gone when the window is to be selected.
    let (lossy, lost) = tmux_that_loses_a_pane_before(t, tmp.path(), "select-window");
    let report = restore::apply(&lossy, &Config::default(), &paths, "default", false)
        .expect("a session that closed under the restore is not a reason for it to stop");
    assert!(lost.exists(), "no pane was ended, so nothing was tested");

    assert!(
        report.created.is_empty(),
        "a session that is not there is not created: {report:#?}"
    );
    assert!(
        report.plan.iter().any(|line| line
            == "stopped restoring session 'alpha': a pane or window of it closed while it was being made; windows made: none; not made: 0 'only'"),
        "the plan must say the session was given up on: {:#?}",
        report.plan
    );
    assert!(alpha_pane(t).is_none(), "the session is gone");
}

#[test]
fn restore_makes_every_session_before_it_types_into_any_pane() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    shells.come_straight_to_the_prompt(t);
    t.run_ok(&["new-session", "-d", "-s", "gamma", "-x", "200", "-y", "50"])
        .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    // Both sessions hold an agent, so there is something to type into each.
    for (session, id) in [("alpha", "abc"), ("gamma", "def")] {
        let pane = t.display(&format!("{session}:"), "#{pane_id}").unwrap();
        t.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
        t.set_pane_option(&pane, "@agent_session_id", id).unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();
    t.run_ok(&["kill-session", "-t", "gamma"]).unwrap();

    let (watched, commands) = tmux_that_writes_down_its_commands(t, tmp.path());
    let report = restore::apply(&watched, &Config::default(), &paths, "default", false).unwrap();
    assert_eq!(report.resumed.len(), 2, "report: {report:#?}");

    let log = std::fs::read_to_string(&commands).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let last_session = lines
        .iter()
        .rposition(|line| line.contains(" new-session "))
        .expect("restore made no session");
    let first_typed = lines
        .iter()
        .position(|line| line.contains(" send-keys "))
        .expect("restore typed nothing");
    assert!(
        last_session < first_typed,
        "a command was typed (line {first_typed}) before the last session was made (line {last_session}):\n{log}"
    );
}

// ---------------------------------------------------------------------------------------
// A restore holds autosave off while it runs, and saves once when it is done
//
// The plugin's hooks autosave on every new window and split, and a restore makes both by the
// dozen: a snapshot taken in the middle of one holds a half-made layout, and was written over
// the snapshot the restore was reading.

/// What `show-options` says of the server option a restore sets while it runs, `""` when it is
/// not set.
fn restoring_option(t: &Tmux) -> String {
    t.run(&["show-options", "-gqv", taw::session::RESTORING_OPTION])
        .unwrap()
}

/// The restore makes alpha, and types its `claude --resume`, before gamma fails: the panes find the
/// fake `claude` of [`Shells`] first, so no agent the machine has installed runs in a test pane.
#[test]
fn a_restore_that_fails_lets_autosave_go_again() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    shells.come_straight_to_the_prompt(t);
    save_alpha_and_gamma(t, &paths, "not a layout");

    restore::apply(t, &Config::default(), &paths, "default", false)
        .expect_err("a layout tmux cannot read is an error");
    assert_eq!(
        restoring_option(t),
        "",
        "a restore that failed left autosave held off"
    );
}

#[test]
fn autosave_if_due_waits_while_a_restore_is_running_and_saves_once_it_is_over() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Set by a restore that is running: this process, as far as the mark can tell.
    taw::session::autosave_touch(&paths).unwrap();
    t.set_global_option(
        taw::session::RESTORING_OPTION,
        &taw::session::restoring_mark(std::process::id(), now),
    )
    .unwrap();
    let held =
        taw::session::autosave_if_due(t, &Config::default(), &paths, std::time::Duration::ZERO)
            .unwrap();
    assert!(!held, "a snapshot was taken in the middle of a restore");
    assert!(
        snapshot::list(&paths).unwrap().is_empty(),
        "a snapshot was written in the middle of a restore"
    );

    // The request is not lost: it is kept, for the save once the restore is over.
    t.unset_global_option(taw::session::RESTORING_OPTION)
        .unwrap();
    assert!(
        taw::session::autosave_if_due(t, &Config::default(), &paths, std::time::Duration::ZERO)
            .unwrap(),
        "the autosave that was asked for was not made once the restore was over"
    );
}

#[test]
fn autosave_if_due_ignores_a_mark_an_hour_old_even_when_its_process_id_is_in_use() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let an_hour_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 3600;

    // A process id that something else has been given since: this test's own, which is there.
    taw::session::autosave_touch(&paths).unwrap();
    t.set_global_option(
        taw::session::RESTORING_OPTION,
        &taw::session::restoring_mark(std::process::id(), an_hour_ago),
    )
    .unwrap();
    assert!(
        taw::session::autosave_if_due(t, &Config::default(), &paths, std::time::Duration::ZERO)
            .unwrap(),
        "a restore that ended an hour ago kept autosave off for good"
    );
}

#[test]
fn autosave_if_due_ignores_a_mark_whose_process_has_gone_however_recent_it_is() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // What a restore that was killed a moment ago leaves: its mark, and no process.
    let mut killed = std::process::Command::new("true").spawn().unwrap();
    let pid = killed.id();
    killed.wait().unwrap();
    taw::session::autosave_touch(&paths).unwrap();
    t.set_global_option(
        taw::session::RESTORING_OPTION,
        &taw::session::restoring_mark(pid, now),
    )
    .unwrap();
    assert!(
        taw::session::autosave_if_due(t, &Config::default(), &paths, std::time::Duration::ZERO)
            .unwrap(),
        "a restore that was killed kept autosave off"
    );
}

#[test]
fn restore_holds_autosave_off_while_it_waits_on_a_pane_and_lets_go_when_it_is_over() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.ask_first(t);

    assert_eq!(
        restoring_option(t),
        "",
        "precondition: no restore is running"
    );
    let restoring = restore_in_the_background(t, paths_dir.path(), Default::default());
    let mut pane = String::new();
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            match alpha_pane(t) {
                Some(id)
                    if t.capture_pane(&id, false)
                        .is_ok_and(|s| s.contains("Trust them?")) =>
                {
                    pane = id;
                    true
                }
                _ => false,
            }
        }),
        "the restored pane never showed the question"
    );
    // Named by the process that is restoring, which here is this one.
    assert!(
        restoring_option(t).starts_with(&format!("{} ", std::process::id())),
        "autosave was not held off while the restore waited: {:?}",
        restoring_option(t)
    );

    t.run_ok(&["send-keys", "-t", &pane, "y", "Enter"]).unwrap();
    restoring.join().unwrap().unwrap();
    assert_eq!(
        restoring_option(t),
        "",
        "the restore left autosave held off"
    );
}

#[test]
fn restore_saves_the_layout_once_it_is_made_with_the_agents_it_resumed_in_it() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    save_alpha_with_a_claude_pane(t, &paths);
    shells.come_straight_to_the_prompt(t);
    // Older than any autosave's debounce, so the snapshot is only rewritten if a restore does.
    let saved = paths.sessions_dir.join("default.json");
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&saved)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    // A hook asked for an autosave while the restore ran, as every new window and split does.
    taw::session::autosave_touch(&paths).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();
    assert_eq!(report.created, vec!["alpha".to_string()], "{report:#?}");

    assert!(
        std::fs::metadata(&saved).unwrap().modified().unwrap() > long_ago,
        "the layout the restore made was not saved"
    );
    let snap = snapshot::load(&paths, "default").unwrap();
    let alpha = snap
        .sessions
        .iter()
        .find(|sess| sess.name == "alpha")
        .unwrap();
    let pane = &alpha.windows[0].panes[0];
    assert_eq!(
        (pane.agent_kind.as_deref(), pane.agent_session_id.as_deref()),
        (Some("claude"), Some("abc")),
        "the saved layout lost the agent the restore had just resumed"
    );
}

#[test]
fn restore_saves_the_session_path_a_pane_was_resumed_from() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    // `pi` and `omp` report the file their conversation is kept in and no id: the pane holds a
    // path, and nothing else that says which conversation it was.
    let session_file = tmp.path().join("pi-session.json").display().to_string();
    save_alpha_with_a_pane_holding(
        t,
        &paths,
        &[
            ("@agent_kind", "pi"),
            ("@agent_session_path", &session_file),
        ],
    );
    shells.come_straight_to_the_prompt(t);
    let saved = paths.sessions_dir.join("default.json");
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&saved)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    // A hook asked for an autosave while the restore ran, as every new window and split does.
    taw::session::autosave_touch(&paths).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();
    let pane = alpha_pane(t).expect("alpha was restored");
    assert_eq!(report.resumed, vec![pane.clone()], "{report:#?}");
    assert!(
        poll_until(std::time::Duration::from_secs(15), || {
            std::fs::read_to_string(shells.pi_argv())
                .is_ok_and(|c| c.contains("--session") && c.contains(&session_file))
        }),
        "the pane was never resumed from its session path (argv: {:?})",
        std::fs::read_to_string(shells.pi_argv())
    );

    // The pane holds the path it was resumed from, as it holds the id of a pane resumed by one...
    assert_eq!(
        t.display(&pane, "#{@agent_session_path}").unwrap(),
        session_file,
        "the resumed pane does not hold the path it was resumed from"
    );
    // ...so that what the restore saves still says which conversation it is.
    assert!(
        std::fs::metadata(&saved).unwrap().modified().unwrap() > long_ago,
        "the layout the restore made was not saved"
    );
    let snap = snapshot::load(&paths, "default").unwrap();
    let pane = &snap
        .sessions
        .iter()
        .find(|sess| sess.name == "alpha")
        .unwrap()
        .windows[0]
        .panes[0];
    assert_eq!(
        (
            pane.agent_kind.as_deref(),
            pane.agent_session_id.as_deref(),
            pane.agent_session_path.as_deref()
        ),
        (Some("pi"), None, Some(session_file.as_str())),
        "the saved layout lost the path of the pane the restore had just resumed, and the next restore resumes nothing"
    );
}

#[test]
fn restore_saves_nothing_when_no_autosave_was_asked_for() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    save_alpha_with_a_claude_pane(t, &paths);
    shells.come_straight_to_the_prompt(t);
    let saved = paths.sessions_dir.join("default.json");
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&saved)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    // No hook is installed and no watcher has reported anything: nothing asked for a save, as
    // on a machine that restores snapshots by hand. What it restores from is not rewritten.
    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();
    assert_eq!(report.created, vec!["alpha".to_string()], "{report:#?}");
    assert_eq!(
        std::fs::metadata(&saved).unwrap().modified().unwrap(),
        long_ago,
        "a restore rewrote the snapshot with no autosave asked for"
    );
}

#[test]
fn restore_does_not_save_over_the_snapshot_when_it_left_a_pane_alone() {
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    save_alpha_with_a_claude_pane(t, &paths);
    shells.ask_first(t);
    let saved = paths.sessions_dir.join("default.json");
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&saved)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    // An autosave was asked for while it ran, and nobody answers the question: the pane is left
    // alone, with its command in the plan.
    taw::session::autosave_touch(&paths).unwrap();
    let report = restore::apply_with(
        t,
        &Config::default(),
        &paths,
        "default",
        false,
        &taw::session::prompt::Timing {
            bound: std::time::Duration::from_secs(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.not_resumed.len(), 1, "report: {report:#?}");

    // The agent is in the snapshot and in no pane. Saved over, it would be gone for good.
    assert_eq!(
        std::fs::metadata(&saved).unwrap().modified().unwrap(),
        long_ago,
        "the snapshot was saved over although a pane was left alone"
    );
    let snap = snapshot::load(&paths, "default").unwrap();
    let pane = &snap
        .sessions
        .iter()
        .find(|sess| sess.name == "alpha")
        .unwrap()
        .windows[0]
        .panes[0];
    assert_eq!(pane.agent_session_id.as_deref(), Some("abc"));
}

// ---------------------------------------------------------------------------------------
// A shell that works in silence before it asks is not at its prompt

/// The real zsh again, with the wait a shell's startup can make in silence: a shell that is alone in
/// front of its terminal, and a shell, and has drawn nothing, is not at its prompt. A test that is
/// told it is would type into the question that comes after the silence, and fail here.
#[test]
fn restore_types_nothing_into_a_zsh_that_works_in_silence_before_it_asks_its_question() {
    if !std::path::Path::new("/bin/zsh").exists() {
        return;
    }
    let s = Server::start();
    let t = &s.0;
    let shells = Shells::new();
    let paths_dir = tempfile::tempdir().unwrap();
    save_alpha_with_a_claude_pane(t, &test_paths(paths_dir.path()));
    shells.ask_after_working_in_silence_in_zsh(t, 1.2);
    restore_waits_for_the_question_to_be_answered(t, &shells, paths_dir.path());
}

// ---------------------------------------------------------------------------------------
// A pane whose saved directory is gone (herdr 0.9.2, #4320)

/// What herdr says of a pane whose saved directory is gone (`src/app/agent_resume.rs` at v0.9.3),
/// word for word.
const UNAVAILABLE: &str =
    "Saved directory is unavailable. Restore the directory and restart this session.";

/// A fake agent called `name` in `dir/bin`, which notes each run it is given in a file of its own in
/// `runs`: the directory it was started in, then its arguments, one to a line. Returns the directory,
/// to put first on the panes' `PATH`.
fn agent_that_notes_its_runs(
    dir: &std::path::Path,
    name: &str,
    runs: &std::path::Path,
) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(runs).unwrap();
    let path = bin.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nrun=$(mktemp '{runs}/run.XXXXXX') && printf '%s\\n' \"$(pwd -P)\" \"$@\" > \
             \"$run.part\" && mv \"$run.part\" \"$run.done\"\n",
            runs = runs.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&path);
    bin
}

/// The runs an [`agent_that_notes_its_runs`] noted, each as the directory and the arguments.
fn runs_of(runs: &std::path::Path) -> Vec<Vec<String>> {
    let mut done: Vec<std::path::PathBuf> = std::fs::read_dir(runs)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "done"))
        .collect();
    done.sort();
    done.iter()
        .map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        })
        .collect()
}

/// Every pane tmux starts from now on finds the fakes in `bin` first on its `PATH`, set from inside the
/// shell as the resume test above explains.
fn starts_panes_with_first(t: &Tmux, bin: &std::path::Path) {
    t.run_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!("export PATH={}:$PATH; exec /bin/sh", bin.display()),
    ])
    .unwrap();
}

/// The panes of `session`, window by window, each window's in their order in it.
fn panes_of(t: &Tmux, session: &str) -> Vec<String> {
    let mut panes: Vec<_> = t
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.session_name == session)
        .collect();
    panes.sort_by_key(|p| (p.window_index, p.pane_index));
    panes.into_iter().map(|p| p.pane_id).collect()
}

// Follows herdr's `failed_deferred_restore_keeps_session_reference_without_retrying_elsewhere`
// (src/app/agent_resume.rs, v0.9.3): a whole restore against a real tmux server, where herdr's test
// starts one pending resume. tmux has made the pane, in the home folder, by the time the directory
// is found gone, so what is checked is that nothing is typed into it, there or anywhere else, and
// that the pane carries herdr's error and the session it saved; a second restore stands for herdr's
// second start, which does nothing. herdr's other case, a shell that cannot start, is tmux's here,
// which closes such a pane, and is left out.
#[test]
fn failed_deferred_restore_keeps_session_reference_without_retrying_elsewhere() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    let runs = tmp.path().join("runs");
    let bin = agent_that_notes_its_runs(tmp.path(), "claude", &runs);
    t.run_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!("export PATH={}:$PATH; exec /bin/sh", bin.display()),
    ])
    .unwrap();
    let (gone, kept) = (tmp.path().join("gone project"), tmp.path().join("kept"));
    for dir in [&gone, &kept] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (gone, kept) = (
        gone.canonicalize().unwrap().display().to_string(),
        kept.canonicalize().unwrap().display().to_string(),
    );

    // Two claude conversations in one window, one in a directory that is about to go; a second
    // session keeps the server up while this one is gone.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &gone,
    ])
    .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "work:", "-c", &kept])
        .unwrap();
    let saved_panes = panes_of(t, "work");
    for (pane, dir, id) in [
        (&saved_panes[0], &gone, "resume-test"),
        (&saved_panes[1], &kept, "kept-session"),
    ] {
        assert!(
            poll_until(std::time::Duration::from_secs(10), || {
                t.display(pane, "#{pane_current_path}").unwrap() == *dir
            }),
            "pane {pane} never came up in {dir}"
        );
        t.set_pane_option(pane, "@agent_kind", "claude").unwrap();
        t.set_pane_option(pane, "@agent_session_id", id).unwrap();
    }
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "work"]).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    assert_eq!(report.created, ["work"], "{report:#?}");
    let restored = panes_of(t, "work");
    assert_eq!(restored.len(), 2, "{report:#?}");
    assert_eq!(report.resumed, [restored[1].clone()], "{report:#?}");
    assert_eq!(report.not_resumed, [restored[0].clone()], "{report:#?}");
    let line = format!(
        "pane {}: saved directory {gone} is gone, so tmux opened it in your home folder and nothing \
         was typed; once the directory is back, resume it with: cd '{gone}' && claude --resume \
         resume-test",
        restored[0]
    );
    assert!(report.plan.contains(&line), "{:#?}", report.plan);

    // The pane in the directory that is still there is resumed: once its run is in, a run in the
    // other pane, typed before it, would be too.
    assert!(
        poll_until(std::time::Duration::from_secs(10), || runs_of(&runs).len()
            == 1),
        "the pane in {kept} was not resumed: {:?}",
        runs_of(&runs)
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        runs_of(&runs),
        [vec![kept.clone(), "--resume".into(), "kept-session".into()]],
        "the agent of the pane whose directory is gone was started somewhere else"
    );

    // The pane carries herdr's error and keeps the session it saved, and nothing marks it as
    // holding an agent: nothing was started in it.
    let pane = &restored[0];
    assert_eq!(
        t.get_pane_option(pane, "@agent_restore_error").unwrap(),
        Some(UNAVAILABLE.to_string())
    );
    assert_eq!(t.get_pane_option(pane, "@agent_kind").unwrap(), None);
    assert_eq!(t.get_pane_option(pane, "@agent_kind_hint").unwrap(), None);
    let home = s.1.path().canonicalize().unwrap().display().to_string();
    assert_eq!(t.display(pane, "#{pane_current_path}").unwrap(), home);
    // What a save takes of it is the pane as it was saved, the session it held with it, and not the
    // home folder tmux opened it in, with no agent.
    let taken = snapshot::take(t).unwrap();
    let saved = &saved_session(&taken, "work").windows[0].panes[0];
    assert_eq!(
        (
            saved.cwd.as_str(),
            saved.agent_kind.as_deref(),
            saved.agent_session_id.as_deref()
        ),
        (gone.as_str(), Some("claude"), Some("resume-test"))
    );

    // Restoring again starts nothing: the session is there, and is left as it is.
    let again = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();
    assert!(again.created.is_empty(), "{again:#?}");
    assert!(again.skipped.contains(&"work".to_string()), "{again:#?}");
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(runs_of(&runs).len(), 1, "{:?}", runs_of(&runs));
}

/// The session `name` of a snapshot.
fn saved_session<'a>(snap: &'a snapshot::Snapshot, name: &str) -> &'a snapshot::SessionSnap {
    snap.sessions
        .iter()
        .find(|session| session.name == name)
        .unwrap_or_else(|| panic!("no session '{name}' in {snap:#?}"))
}

// Follows herdr's `failed_cold_restore_preserves_panes_and_saved_directories`
// (src/persist/restore.rs, v0.9.3): the layout is made in a
// real tmux and saved, its sessions closed and restored, and what is checked is what a save takes of
// the restored layout (`snapshot::take`, herdr's `capture`). herdr's pane label is taw's agent name,
// the one name a saved pane keeps. tmux has made the pane, in the home folder, where herdr makes none,
// so "no replacement shell elsewhere" is that the agent's command was typed nowhere. herdr's other
// case, a shell that cannot start, is tmux's here, which closes such a pane, and is left out.
#[test]
fn failed_cold_restore_preserves_panes_and_saved_directories() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    let runs = tmp.path().join("runs");
    let bin = agent_that_notes_its_runs(tmp.path(), "opencode", &runs);
    t.run_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!("export PATH={}:$PATH; exec /bin/sh", bin.display()),
    ])
    .unwrap();
    let (cwd, missing) = (tmp.path().join("cwd"), tmp.path().join("missing directory"));
    for dir in [&cwd, &missing] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (cwd, missing) = (
        cwd.canonicalize().unwrap().display().to_string(),
        missing.canonicalize().unwrap().display().to_string(),
    );

    // Two workspaces of two tabs each, every pane in `cwd` but the first, which is in a directory
    // about to go, with a name and an OpenCode conversation; and a session to keep the server up.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    for (session, first) in [("first", &missing), ("second", &cwd)] {
        t.run_ok(&[
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "200",
            "-y",
            "50",
            "-c",
            first,
        ])
        .unwrap();
        t.run_ok(&[
            "new-window",
            "-d",
            "-t",
            &format!("={session}:"),
            "-c",
            &cwd,
        ])
        .unwrap();
    }
    let failed = panes_of(t, "first")
        .into_iter()
        .find(|pane| t.display(pane, "#{window_index}").unwrap() == "0")
        .unwrap();
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&failed, "#{pane_current_path}").unwrap() == missing
        }),
        "pane {failed} never came up in {missing}"
    );
    t.set_pane_option(&failed, "@agent_kind", "opencode")
        .unwrap();
    t.set_pane_option(&failed, "@agent_session_id", "keep-my-session")
        .unwrap();
    t.set_pane_option(&failed, "@agent_name", "keeper").unwrap();
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    for session in ["first", "second"] {
        t.run_ok(&["kill-session", "-t", &format!("={session}")])
            .unwrap();
    }
    std::fs::remove_dir_all(&missing).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    // A launch failure deletes no workspace and no tab.
    assert_eq!(report.created, ["first", "second"], "{report:#?}");
    let captured = snapshot::take(t).unwrap();
    for name in ["first", "second"] {
        assert_eq!(saved_session(&captured, name).windows.len(), 2, "{name}");
    }
    // The fallback directory does not replace what was saved, and nor does the agent go.
    let pane = &saved_session(&captured, "first").windows[0].panes[0];
    assert_eq!(
        pane.cwd, missing,
        "fallback cwd must not replace saved intent"
    );
    assert_eq!(pane.agent_name.as_deref(), Some("keeper"));
    assert_eq!(pane.agent_kind.as_deref(), Some("opencode"));
    assert_eq!(pane.agent_session_id.as_deref(), Some("keep-my-session"));
    let healthy = &saved_session(&captured, "second").windows[0].panes[0];
    assert_eq!(healthy.cwd, cwd);
    // Nothing was typed into the pane, and no replacement agent was started anywhere else.
    let restored = panes_of(t, "first")
        .into_iter()
        .find(|pane| t.display(pane, "#{window_index}").unwrap() == "0")
        .unwrap();
    assert_eq!(
        t.get_pane_option(&restored, "@agent_restore_error")
            .unwrap(),
        Some(UNAVAILABLE.to_string())
    );
    assert_eq!(
        report.not_resumed,
        std::slice::from_ref(&restored),
        "{report:#?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(runs_of(&runs).is_empty(), "{:?}", runs_of(&runs));
}

/// What a pane whose saved directory is gone keeps of itself goes in one tmux command with herdr's
/// error: a pane whose agent reported a command that the checks let through, 8000 quotes long, is too
/// long for one once written as the JSON a save reads back, and the pane keeps the rest of itself, its
/// session included, without the command.
#[test]
fn a_pane_whose_saved_self_is_too_long_for_one_tmux_command_keeps_all_but_its_reported_command() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    let runs = tmp.path().join("runs");
    starts_panes_with_first(
        t,
        &agent_that_notes_its_runs(tmp.path(), "prime-agent", &runs),
    );
    let gone = tmp.path().join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap().display().to_string();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &gone,
    ])
    .unwrap();
    let pane = panes_of(t, "work").remove(0);
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&pane, "#{pane_current_path}").unwrap() == gone
        }),
        "pane {pane} never came up in {gone}"
    );
    let mut snap = snapshot::take(t).unwrap();
    let argv = vec!["prime-agent".to_string(), "\"".repeat(8000)];
    assert!(taw::agent_resume::validate_argv(&argv).is_ok());
    let saved = snap
        .sessions
        .iter_mut()
        .find(|session| session.name == "work")
        .unwrap();
    saved.windows[0].panes[0].agent_kind = Some("prime-agent".to_string());
    saved.windows[0].panes[0].agent_session_id = Some("s1".to_string());
    saved.windows[0].panes[0].agent_resume = Some(taw::agent_resume::ReportedResume {
        source: "prime-agent".to_string(),
        agent: "prime-agent".to_string(),
        argv,
    });
    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();

    restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let restored = panes_of(t, "work").remove(0);
    assert_eq!(
        t.get_pane_option(&restored, "@agent_restore_error")
            .unwrap(),
        Some(UNAVAILABLE.to_string())
    );
    let taken = snapshot::take(t).unwrap();
    let kept = &saved_session(&taken, "work").windows[0].panes[0];
    assert_eq!(
        (
            kept.cwd.as_str(),
            kept.agent_kind.as_deref(),
            kept.agent_session_id.as_deref(),
            kept.agent_resume.as_ref()
        ),
        (gone.as_str(), Some("prime-agent"), Some("s1"), None)
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(runs_of(&runs).is_empty(), "{:?}", runs_of(&runs));
}

/// With `[session] resume_agents_on_restore = false`, a pane whose saved directory is gone has no
/// command to run, and is not in `not_resumed`: the plan says only which directory is gone. It is
/// marked all the same, and the next save keeps it as it was saved.
#[test]
fn with_resume_off_a_pane_whose_saved_directory_is_gone_is_said_so_and_kept_all_the_same() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    let runs = tmp.path().join("runs");
    starts_panes_with_first(t, &agent_that_notes_its_runs(tmp.path(), "claude", &runs));
    let gone = tmp.path().join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap().display().to_string();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &gone,
    ])
    .unwrap();
    let pane = panes_of(t, "work").remove(0);
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&pane, "#{pane_current_path}").unwrap() == gone
        }),
        "pane {pane} never came up in {gone}"
    );
    t.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    t.set_pane_option(&pane, "@agent_session_id", "abc")
        .unwrap();
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let mut off = Config::default();
    off.session.resume_agents_on_restore = false;

    let report = restore::apply(t, &off, &paths, "default", false).unwrap();

    let restored = panes_of(t, "work").remove(0);
    assert!(report.not_resumed.is_empty(), "{report:#?}");
    let line = format!(
        "pane {restored}: saved directory {gone} is gone, so tmux opened it in your home folder"
    );
    assert!(report.plan.contains(&line), "{:#?}", report.plan);
    assert_eq!(
        t.get_pane_option(&restored, "@agent_restore_error")
            .unwrap(),
        Some(UNAVAILABLE.to_string())
    );
    let taken = snapshot::take(t).unwrap();
    let kept = &saved_session(&taken, "work").windows[0].panes[0];
    assert_eq!(
        (kept.cwd.as_str(), kept.agent_session_id.as_deref()),
        (gone.as_str(), Some("abc"))
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(runs_of(&runs).is_empty(), "{:?}", runs_of(&runs));
}

/// `dir` as tmux reads it back from a `-c` this test passes it itself: `#` written `##` (tmux expands
/// formats there) and a trailing `;` written `\;` (tmux ends a command at a word that ends in one).
fn as_tmux_reads_it(dir: &str) -> String {
    let escaped = dir.replace('#', "##");
    match escaped.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => escaped,
    }
}

/// A folder whose name tmux reads as a format (`C#Sharp`, where `#S` is the session's name) and one
/// whose name ends in `;` (where tmux ends a command) are restored as themselves, and the agent saved in
/// each is resumed there. Given as they are, the first opened in the home folder, where its resume was
/// typed, and the second ended the restore with tmux's `unknown command`.
#[test]
fn a_folder_with_a_hash_or_a_trailing_semicolon_in_its_name_is_restored_as_itself() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    let runs = tmp.path().join("runs");
    starts_panes_with_first(t, &agent_that_notes_its_runs(tmp.path(), "claude", &runs));
    // The first pane of the session, a pane split from it, and the first pane of a second window:
    // each is made by a command of its own.
    let folders: Vec<String> = ["C#Sharp", "semi;", "D#Srive"]
        .iter()
        .map(|name| {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir.canonicalize().unwrap().display().to_string()
        })
        .collect();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &as_tmux_reads_it(&folders[0]),
    ])
    .unwrap();
    t.run_ok(&[
        "split-window",
        "-d",
        "-t",
        "=work:",
        "-c",
        &as_tmux_reads_it(&folders[1]),
    ])
    .unwrap();
    t.run_ok(&[
        "new-window",
        "-d",
        "-t",
        "=work:",
        "-c",
        &as_tmux_reads_it(&folders[2]),
    ])
    .unwrap();
    let saved = panes_of(t, "work");
    assert_eq!(saved.len(), 3);
    for (n, (pane, dir)) in saved.iter().zip(&folders).enumerate() {
        assert!(
            poll_until(std::time::Duration::from_secs(10), || {
                t.display(pane, "#{pane_current_path}").unwrap() == *dir
            }),
            "pane {pane} never came up in {dir}"
        );
        t.set_pane_option(pane, "@agent_kind", "claude").unwrap();
        t.set_pane_option(pane, "@agent_session_id", &format!("s-{n}"))
            .unwrap();
    }
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let restored = panes_of(t, "work");
    let mut resumed = report.resumed.clone();
    resumed.sort();
    let mut all = restored.clone();
    all.sort();
    assert_eq!(resumed, all, "{report:#?}");
    assert!(report.not_resumed.is_empty(), "{report:#?}");
    assert!(
        poll_until(std::time::Duration::from_secs(10), || runs_of(&runs).len()
            == 3),
        "{:?}",
        runs_of(&runs)
    );
    let mut ran = runs_of(&runs);
    ran.sort();
    let mut want: Vec<Vec<String>> = folders
        .iter()
        .enumerate()
        .map(|(n, dir)| vec![dir.clone(), "--resume".to_string(), format!("s-{n}")])
        .collect();
    want.sort();
    assert_eq!(ran, want);
}

/// A tmux of this test's own that does not open a folder as it is asked to: it hands the real one, at
/// `t`, every word that names something in `root` with taw's `##` read back as `#`, as every tmux was
/// handed such a folder before taw wrote `#` as `##`, so that a folder called `C#Sharp` opens in
/// `C<the session's name>harp`.
fn a_tmux_that_reads_a_hash_in_a_folder_as_a_format(t: &Tmux, root: &std::path::Path) -> Tmux {
    let path = root.join("tmux");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor word; do\n  shift\n  case $word in\n    '{root}'/*) word=$(printf '%s\\n' \
             \"$word\" | /usr/bin/sed 's/##/#/g') ;;\n  esac\n  set -- \"$@\" \"$word\"\ndone\nexec \
             '{tmux}' \"$@\"\n",
            root = root.display(),
            tmux = t.bin.display(),
        ),
    )
    .unwrap();
    crate::common::make_executable(&path);
    Tmux {
        bin: path,
        socket: t.socket.clone(),
    }
}

/// `taw session restore`'s line that runs `command` from `dir`: the `cd` quoted as the words of a
/// reported command are.
fn from_directory(dir: &str, command: &str) -> String {
    format!(
        "{} && {command}",
        taw::agent_resume::command_line(&["cd".to_string(), dir.to_string()])
    )
}

/// A pane that tmux does not open in its saved directory, though that directory is there, is left
/// alone as a pane whose directory is gone is: nothing is typed into it, it is marked so, the plan says
/// where tmux opened it and the command to run from its own directory, and the next save keeps that
/// directory. The tmux here reads a `#` in a folder's name as a format, as every tmux did before taw
/// wrote it `##`: one folder reads as another that is there, which tmux opens, and one as a folder that
/// is not, for which tmux opens the home folder.
#[test]
fn a_pane_that_tmux_opens_somewhere_else_than_its_saved_directory_is_left_alone() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let paths = test_paths(&root.join("taw"));
    let runs = root.join("runs");
    starts_panes_with_first(t, &agent_that_notes_its_runs(&root, "claude", &runs));
    // `#S` is the session's name: `C#Sharp` reads as `Cworkharp`, which is there, and `E#Sdge` as a
    // folder that is not, whatever the name is.
    let [edge, sharp, plain, elsewhere] = ["E#Sdge", "C#Sharp", "plain", "Cworkharp"].map(|name| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.display().to_string()
    });
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    // The first pane of the session, a pane split from it, and the first pane of a second window: each
    // is made by a command of its own.
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &as_tmux_reads_it(&edge),
    ])
    .unwrap();
    t.run_ok(&[
        "split-window",
        "-d",
        "-t",
        "=work:",
        "-c",
        &as_tmux_reads_it(&sharp),
    ])
    .unwrap();
    t.run_ok(&[
        "new-window",
        "-d",
        "-t",
        "=work:",
        "-c",
        &as_tmux_reads_it(&plain),
    ])
    .unwrap();
    let saved = panes_of(t, "work");
    assert_eq!(saved.len(), 3);
    for (n, (pane, dir)) in saved.iter().zip([&edge, &sharp, &plain]).enumerate() {
        assert!(
            poll_until(std::time::Duration::from_secs(10), || {
                t.display(pane, "#{pane_current_path}").unwrap() == *dir
            }),
            "pane {pane} never came up in {dir}"
        );
        t.set_pane_option(pane, "@agent_kind", "claude").unwrap();
        t.set_pane_option(pane, "@agent_session_id", &format!("s-{n}"))
            .unwrap();
    }
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();

    let lying = a_tmux_that_reads_a_hash_in_a_folder_as_a_format(t, &root);
    let report = restore::apply(&lying, &Config::default(), &paths, "default", false).unwrap();

    let restored = panes_of(t, "work");
    assert_eq!(restored.len(), 3, "{report:#?}");
    let home = s.1.path().canonicalize().unwrap().display().to_string();
    for (pane, opened_in) in [(&restored[0], &home), (&restored[1], &elsewhere)] {
        assert!(
            poll_until(std::time::Duration::from_secs(10), || {
                t.display(pane, "#{pane_current_path}").unwrap() == *opened_in
            }),
            "pane {pane} was not opened in {opened_in}"
        );
    }
    assert_eq!(report.resumed, [restored[2].clone()], "{report:#?}");
    assert_eq!(report.not_resumed, restored[..2], "{report:#?}");
    for line in [
        format!(
            "pane {}: tmux opened it in your home folder and not in its saved directory {edge}, so \
             nothing was typed; resume it with: {}",
            restored[0],
            from_directory(&edge, "claude --resume s-0")
        ),
        format!(
            "pane {}: tmux opened it in {elsewhere} and not in its saved directory {sharp}, so nothing \
             was typed; resume it with: {}",
            restored[1],
            from_directory(&sharp, "claude --resume s-1")
        ),
    ] {
        assert!(report.plan.contains(&line), "{line}\n{:#?}", report.plan);
    }
    for (pane, mark) in restored
        .iter()
        .zip([Some(UNAVAILABLE), Some(UNAVAILABLE), None])
    {
        assert_eq!(
            t.get_pane_option(pane, "@agent_restore_error").unwrap(),
            mark.map(str::to_string),
            "{pane}"
        );
    }

    // The pane whose directory tmux did open is resumed there, and nothing ran anywhere else.
    assert!(
        poll_until(std::time::Duration::from_secs(10), || runs_of(&runs).len()
            == 1),
        "{:?}",
        runs_of(&runs)
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        runs_of(&runs),
        [vec![plain.clone(), "--resume".into(), "s-2".into()]]
    );
    // What a save takes of the panes left alone is their saved directory and session.
    let taken = snapshot::take(t).unwrap();
    let kept: Vec<_> = saved_session(&taken, "work").windows[0]
        .panes
        .iter()
        .map(|pane| (pane.cwd.clone(), pane.agent_session_id.clone()))
        .collect();
    assert_eq!(
        kept,
        [
            (edge.clone(), Some("s-0".to_string())),
            (sharp.clone(), Some("s-1".to_string()))
        ]
    );
}

/// A folder given no permissions at all, which is given them back when this is dropped, so that the
/// test's temporary directory can be removed.
struct Shut(std::path::PathBuf);

impl Shut {
    fn new(dir: &std::path::Path) -> Shut {
        set_mode(dir, 0o000);
        Shut(dir.to_path_buf())
    }
}

impl Drop for Shut {
    fn drop(&mut self) {
        set_mode(&self.0, 0o755);
    }
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A pane whose saved directory is there but cannot be opened (no permission to go into it) is opened by
/// tmux in the home folder, and is left alone as a pane whose directory is gone is: nothing is typed into
/// it, and the plan says where tmux opened it.
#[test]
fn a_pane_whose_saved_directory_cannot_be_opened_is_left_alone() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let paths = test_paths(&root.join("taw"));
    let runs = root.join("runs");
    starts_panes_with_first(t, &agent_that_notes_its_runs(&root, "claude", &runs));
    let shut = root.join("shut");
    std::fs::create_dir_all(&shut).unwrap();
    let shut = shut.display().to_string();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &shut,
    ])
    .unwrap();
    let pane = panes_of(t, "work").remove(0);
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&pane, "#{pane_current_path}").unwrap() == shut
        }),
        "pane {pane} never came up in {shut}"
    );
    t.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    t.set_pane_option(&pane, "@agent_session_id", "s-0")
        .unwrap();
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();
    let closed = Shut::new(std::path::Path::new(&shut));
    if std::fs::read_dir(&closed.0).is_ok() {
        eprintln!("skipped: this user opens a folder with no permissions (root)");
        return;
    }

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let restored = panes_of(t, "work").remove(0);
    let home = s.1.path().canonicalize().unwrap().display().to_string();
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&restored, "#{pane_current_path}").unwrap() == home
        }),
        "pane {restored} was not opened in the home folder"
    );
    assert!(report.resumed.is_empty(), "{report:#?}");
    assert_eq!(
        report.not_resumed,
        std::slice::from_ref(&restored),
        "{report:#?}"
    );
    let line = format!(
        "pane {restored}: tmux opened it in your home folder and not in its saved directory {shut}, so \
         nothing was typed; resume it with: {}",
        from_directory(&shut, "claude --resume s-0")
    );
    assert!(report.plan.contains(&line), "{line}\n{:#?}", report.plan);
    assert_eq!(
        t.get_pane_option(&restored, "@agent_restore_error")
            .unwrap(),
        Some(UNAVAILABLE.to_string())
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(runs_of(&runs).is_empty(), "{:?}", runs_of(&runs));
}

/// One session saved in a pane whose directory is gone and in a pane after it whose directory is there
/// is resumed in the second: the pane left alone takes no part in which pane resumes a session, and the
/// plan says of no pane that another resumes its session.
#[test]
fn a_session_saved_in_a_gone_pane_and_in_a_pane_after_it_is_resumed_in_the_second() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let paths = test_paths(&root.join("taw"));
    let runs = root.join("runs");
    starts_panes_with_first(t, &agent_that_notes_its_runs(&root, "claude", &runs));
    let [gone, kept] = ["gone", "kept"].map(|name| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.display().to_string()
    });
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &gone,
    ])
    .unwrap();
    t.run_ok(&["split-window", "-d", "-t", "=work:", "-c", &kept])
        .unwrap();
    let saved = panes_of(t, "work");
    for (pane, dir) in saved.iter().zip([&gone, &kept]) {
        assert!(
            poll_until(std::time::Duration::from_secs(10), || {
                t.display(pane, "#{pane_current_path}").unwrap() == *dir
            }),
            "pane {pane} never came up in {dir}"
        );
        t.set_pane_option(pane, "@agent_kind", "claude").unwrap();
        t.set_pane_option(pane, "@agent_session_id", "shared")
            .unwrap();
    }
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let restored = panes_of(t, "work");
    assert_eq!(report.resumed, [restored[1].clone()], "{report:#?}");
    assert_eq!(report.not_resumed, [restored[0].clone()], "{report:#?}");
    assert!(
        !report
            .plan
            .iter()
            .any(|line| line.contains("resumes the same session")),
        "{:#?}",
        report.plan
    );
    assert!(
        poll_until(std::time::Duration::from_secs(10), || runs_of(&runs).len()
            == 1),
        "{:?}",
        runs_of(&runs)
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        runs_of(&runs),
        [vec![kept.clone(), "--resume".into(), "shared".into()]]
    );
}

/// A pane with no agent whose saved directory is gone keeps that directory in the next save, and not the
/// home folder tmux opened it in.
#[test]
fn a_pane_with_no_agent_whose_saved_directory_is_gone_keeps_it_in_the_next_save() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let paths = test_paths(&root.join("taw"));
    let gone = root.join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    let gone = gone.display().to_string();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "200",
        "-y",
        "50",
        "-c",
        &gone,
    ])
    .unwrap();
    let pane = panes_of(t, "work").remove(0);
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&pane, "#{pane_current_path}").unwrap() == gone
        }),
        "pane {pane} never came up in {gone}"
    );
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", "=work"]).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let restored = panes_of(t, "work").remove(0);
    let line = format!(
        "pane {restored}: saved directory {gone} is gone, so tmux opened it in your home folder"
    );
    assert!(report.plan.contains(&line), "{:#?}", report.plan);
    let home = s.1.path().canonicalize().unwrap().display().to_string();
    assert!(
        poll_until(std::time::Duration::from_secs(10), || {
            t.display(&restored, "#{pane_current_path}").unwrap() == home
        }),
        "pane {restored} was not opened in the home folder"
    );
    let taken = snapshot::take(t).unwrap();
    let kept = &saved_session(&taken, "work").windows[0].panes[0];
    assert_eq!(
        (kept.cwd.as_str(), kept.agent_kind.as_deref()),
        (gone.as_str(), None)
    );
}

/// Sessions and windows whose names hold a `#` or end in `;` are restored under those names, and a
/// second restore finds the sessions the first made and leaves them as they are. tmux expands formats
/// in the name it makes or renames a session or window with: a session saved as `C#Sharp` came back
/// as `Charp` and a window `W#Sx` as `W<the session's name>x`, so the next restore did not find the
/// session and made it again; and a name that ends in `;` ended the command. The first window of each
/// session is the one a restore renames, and the second the one it makes with its name.
#[test]
fn sessions_and_windows_named_with_a_hash_or_a_semicolon_are_restored_once_under_their_names() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    let saved = [
        ("C#Sharp", ["W#Sx", "V#{session_name}"]),
        ("semi;", ["w;", "plain"]),
    ];
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    let mut ids = Vec::new();
    for (session, windows) in &saved {
        let id = t
            .run(&[
                "new-session",
                "-d",
                "-s",
                &as_tmux_reads_it(session),
                "-n",
                &as_tmux_reads_it(windows[0]),
                "-x",
                "200",
                "-y",
                "50",
                "-P",
                "-F",
                "#{session_id}",
            ])
            .unwrap();
        t.run_ok(&[
            "new-window",
            "-d",
            "-t",
            &format!("{id}:"),
            "-n",
            &as_tmux_reads_it(windows[1]),
        ])
        .unwrap();
        ids.push(id);
    }
    // Every window of the two sessions, as `<session> <window>`, in order.
    let names = || {
        let listed = t
            .run(&[
                "list-windows",
                "-a",
                "-F",
                "#{session_name}\t#{window_index}\t#{window_name}",
            ])
            .unwrap();
        let mut rows: Vec<(String, u32, String)> = listed
            .lines()
            .filter_map(|line| {
                let mut columns = line.splitn(3, '\t');
                let (session, index, window) = (columns.next()?, columns.next()?, columns.next()?);
                Some((session.to_string(), index.parse().ok()?, window.to_string()))
            })
            .filter(|(session, _, _)| saved.iter().any(|(name, _)| name == session))
            .collect();
        rows.sort();
        rows.into_iter()
            .map(|(session, _, window)| format!("{session} {window}"))
            .collect::<Vec<_>>()
    };
    let made = names();
    assert_eq!(
        made,
        [
            "C#Sharp W#Sx",
            "C#Sharp V#{session_name}",
            "semi; w;",
            "semi; plain"
        ]
    );
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    for id in &ids {
        t.run_ok(&["kill-session", "-t", id]).unwrap();
    }

    let first = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let mut created = first.created.clone();
    created.sort();
    assert_eq!(created, ["C#Sharp", "semi;"], "{first:#?}");
    assert_eq!(names(), made);

    let second = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    assert!(second.created.is_empty(), "{second:#?}");
    for (session, _) in &saved {
        assert!(
            second.skipped.iter().any(|skipped| skipped == session),
            "{second:#?}"
        );
    }
    assert_eq!(names(), made);
}

/// A window saved under a name that starts with a dash comes back under it. A restore renames the first
/// window of a session it makes, and tmux read the dash as a flag (`unknown flag -first`) unless a `--`
/// ended the flags, so the restore of that session stopped at the rename and the rest of it never came.
#[test]
fn a_window_saved_under_a_name_that_starts_with_a_dash_is_restored_under_it() {
    let s = Server::start();
    let t = &s.0;
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("taw"));
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();
    let id = t
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dashes",
            "-n",
            "-first",
            "-x",
            "200",
            "-y",
            "50",
            "-P",
            "-F",
            "#{session_id}",
        ])
        .unwrap();
    t.run_ok(&["new-window", "-d", "-t", &format!("{id}:"), "-n", "-second"])
        .unwrap();
    let names = || {
        let listed = t
            .run(&[
                "list-windows",
                "-t",
                "=dashes:",
                "-F",
                "#{window_index}\t#{window_name}",
            ])
            .unwrap();
        listed
            .lines()
            .filter_map(|line| line.split_once('\t').map(|(_, name)| name.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(names(), ["-first", "-second"]);
    snapshot::save(&paths, "default", &snapshot::take(t).unwrap()).unwrap();
    t.run_ok(&["kill-session", "-t", &id]).unwrap();

    let restored = restore::apply(t, &Config::default(), &paths, "default", false)
        .expect("a window named with a leading dash must not stop the restore");

    assert_eq!(restored.created, ["dashes"], "{restored:#?}");
    assert_eq!(names(), ["-first", "-second"]);
}

/// A sandboxed pane comes back through `taw sandbox`, and a restore marks it with its profile, which
/// has no process until the typed line marks the pane with its own. A line refused before it marks
/// (the prompt check, a profile gone since) would leave that mark for good, and a session kept without
/// the profile would be resumed outside it: the restore takes its own marks off a pane whose line
/// started nothing and whose shell is back in front, says so, and leaves a pane whose line runs as it
/// is. Here `taw` on the panes' `PATH` is a stand-in that refuses for claude and runs on for codex;
/// the `taw` restore names in its line is found on the server's global `PATH`, a link to this test.
#[test]
fn a_restore_takes_its_marks_off_a_sandboxed_pane_whose_line_did_not_start() {
    let s = Server::start();
    let t = &s.0;
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
    ])
    .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let named = tmp.path().join("named");
    let panes_bin = tmp.path().join("bin");
    std::fs::create_dir_all(&named).unwrap();
    std::fs::create_dir_all(&panes_bin).unwrap();
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), named.join("taw")).unwrap();
    let stand_in = panes_bin.join("taw");
    std::fs::write(
        &stand_in,
        "#!/bin/sh\ncase \"$2\" in\n  claude) echo 'taw sandbox: refused' >&2; exit 2 ;;\n  *) exec sleep 30 ;;\nesac\n",
    )
    .unwrap();
    crate::common::make_executable(&stand_in);
    t.run_ok(&[
        "set-environment",
        "-g",
        "PATH",
        &named.display().to_string(),
    ])
    .unwrap();
    t.run_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!(
            "export PATH={}:/usr/bin:/bin; exec /bin/sh",
            panes_bin.display()
        ),
    ])
    .unwrap();

    let first = t
        .list_panes()
        .unwrap()
        .into_iter()
        .find(|p| p.session_name == "alpha")
        .unwrap()
        .pane_id;
    t.run_ok(&["split-window", "-d", "-t", &first]).unwrap();
    let alpha: Vec<String> = t
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.session_name == "alpha")
        .map(|p| p.pane_id)
        .collect();
    assert_eq!(alpha.len(), 2);
    for (pane, kind, id) in [(&alpha[0], "claude", "c-1"), (&alpha[1], "codex", "x-1")] {
        for (option, value) in [
            ("@agent_kind", kind),
            ("@agent_session_id", id),
            ("@agent_sandbox", "default"),
            ("@agent_container", "docker"),
        ] {
            t.set_pane_option(pane, option, value).unwrap();
        }
    }

    let paths_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(paths_dir.path());
    let snap = snapshot::take(t).unwrap();
    snapshot::save(&paths, "default", &snap).unwrap();
    t.run_ok(&["kill-session", "-t", "alpha"]).unwrap();

    let report = restore::apply(t, &Config::default(), &paths, "default", false).unwrap();

    let restored: Vec<String> = t
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.session_name == "alpha")
        .map(|p| p.pane_id)
        .collect();
    let (refused, running) = (&restored[0], &restored[1]);
    assert_eq!(
        &report.resumed,
        std::slice::from_ref(running),
        "{report:#?}"
    );
    assert_eq!(
        &report.not_resumed,
        std::slice::from_ref(refused),
        "{report:#?}"
    );
    assert!(
        report.plan.contains(&format!(
            "claude did not start in its sandbox in pane {refused}: its shell is back at its prompt \
             (taw pane read {refused} shows what the profile said); resume it with: \
             taw sandbox claude -- --resume c-1"
        )),
        "{:#?}",
        report.plan
    );
    // The kept resume too: a later `taw sandbox` in the pane marks it afresh and would carry a stale
    // one into its next save.
    for option in [
        "@agent_sandbox",
        "@agent_sandbox_resume",
        "@agent_kind",
        "@agent_session_id",
        "@agent_acquiring",
        "@agent_kind_hint",
    ] {
        assert_eq!(
            t.get_pane_option(refused, option).unwrap(),
            None,
            "{option}"
        );
    }
    assert_eq!(
        t.get_pane_option(running, "@agent_sandbox").unwrap(),
        Some("default".to_string())
    );
    // The kind and the session it was resumed with are in the kept resume alone, beside the mark.
    let kept: snapshot::KeptResume = serde_json::from_str(
        &t.get_pane_option(running, "@agent_sandbox_resume")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (kept.kind.as_str(), kept.session_id.as_deref()),
        ("codex", Some("x-1"))
    );
    for option in ["@agent_kind", "@agent_session_id", "@agent_kind_hint"] {
        assert_eq!(
            t.get_pane_option(running, option).unwrap(),
            None,
            "{option}"
        );
    }
}
