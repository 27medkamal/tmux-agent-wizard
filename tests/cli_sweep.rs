//! `scripts/cli-sweep.sh` runs every verb of the CLI against a throwaway tmux server, `taw integration
//! install` and `uninstall` among them, and whoever is changing the verbs runs it by hand. It has to
//! leave that person's machine as it found it: nothing of their home, of the config directory of an
//! agent or of their zoxide database read or written, and nothing of their environment passed on.
//!
//! This runs it the way such a person would, with a machine of decoys in the environment: a home, a
//! directory for each agent and for zoxide, each with a file in it that an install or a shell would
//! change (devin's config, a shell history), a tmux socket directory, and a secret in a variable no
//! verb has a use for. The sweep may write in two places and no other: a scratch directory in
//! `$TMPDIR`, and a tmux socket directory of its own in `/tmp` (short, as a socket's path has to be),
//! and it removes both when it ends. Every `taw` it runs is the test's wrapper around the real one,
//! which notes the names of the environment variables it was started with (never a value but `$HOME`
//! and `$TMUX_TMPDIR`, which are the sweep's own).

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use common::proxy::Proxy;

const SWEEP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/cli-sweep.sh");

/// A made-up variable, which stands for any of the caller's that no verb has a use for: a token, an
/// agent's home directory that the sweep does not list.
const DECOY_VARIABLE: &str = "TAW_SWEEP_TEST_DECOY_SECRET";

/// What a file or a directory is, as far as saying that it was not touched.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File(Vec<u8>, SystemTime),
    Other,
}

/// Every path under `root` (itself left out) from `root`, and what it is.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<PathBuf, Entry>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.is_dir() {
                found.insert(relative, Entry::Dir);
                walk(root, &path, found);
            } else if meta.is_file() {
                let content = std::fs::read(&path).unwrap();
                found.insert(relative, Entry::File(content, meta.modified().unwrap()));
            } else {
                found.insert(relative, Entry::Other);
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(root, root, &mut found);
    found
}

/// What differs between two snapshots, one line a path.
fn drift(before: &BTreeMap<PathBuf, Entry>, after: &BTreeMap<PathBuf, Entry>) -> Vec<String> {
    let mut lines = Vec::new();
    for (path, entry) in after {
        match before.get(path) {
            None => lines.push(format!("created {}", path.display())),
            Some(was) if was != entry => lines.push(format!("changed {}", path.display())),
            Some(_) => {}
        }
    }
    for path in before.keys().filter(|path| !after.contains_key(*path)) {
        lines.push(format!("removed {}", path.display()));
    }
    lines
}

/// A file with `content` at `path`, its directories made.
fn put(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn the_sweep_leaves_the_machine_of_whoever_runs_it_as_it_found_it() {
    if which::which("python3").is_err() {
        eprintln!("skipped: the sweep needs python3 to read the envelopes");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();

    // The machine of whoever runs it. Devin's config is where an install would write.
    let machine = root.join("machine");
    put(&machine.join("home/.bash_history"), "ls\n");
    put(&machine.join("home/.config/devin/config.json"), "{}\n");
    put(&machine.join("claude/settings.json"), "{\"decoy\":true}\n");
    put(&machine.join("codex/config.toml"), "# decoy\n");
    put(&machine.join("xdg-config/devin/config.json"), "{}\n");
    put(&machine.join("xdg-data/keep"), "x\n");
    put(&machine.join("xdg-state/keep"), "x\n");
    put(&machine.join("zoxide/keep"), "x\n");

    // The caller's tmux socket directory, which the sweep has no use for: it makes one of its own.
    let tmux_tmp = machine.join("tmux-tmp");
    std::fs::create_dir_all(&tmux_tmp).unwrap();
    let before = snapshot(&machine);

    // Where the sweep may write, and the wrapper that notes what each `taw` it runs is started with.
    let tmp = root.join("tmp");
    let seen = root.join("seen.txt");
    std::fs::create_dir_all(&tmp).unwrap();
    let wrapper = root.join("bin/taw");
    put(
        &wrapper,
        &format!(
            "#!/bin/sh\n{{ env | cut -d= -f1; printf 'HOME=%s\\nTMUX_TMPDIR=%s\\n' \"$HOME\" \"$TMUX_TMPDIR\"; }} >> '{}'\nexec '{}' \"$@\"\n",
            seen.display(),
            env!("CARGO_BIN_EXE_taw")
        ),
    );
    {
        crate::common::make_executable(&wrapper);
    }

    // A listener where the proxy of the sweep's processes is, to hear if one of them reaches out.
    let proxy = Proxy::start();
    let out = Command::new("bash")
        .arg(SWEEP)
        .arg(&wrapper)
        .env_clear()
        .env("TAW_SWEEP_PROXY", proxy.url())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TERM", "dumb")
        .env("TMPDIR", &tmp)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env("HOME", machine.join("home"))
        .env("CLAUDE_CONFIG_DIR", machine.join("claude"))
        .env("CODEX_HOME", machine.join("codex"))
        .env("XDG_CONFIG_HOME", machine.join("xdg-config"))
        .env("XDG_DATA_HOME", machine.join("xdg-data"))
        .env("XDG_STATE_HOME", machine.join("xdg-state"))
        .env("_ZO_DATA_DIR", machine.join("zoxide"))
        .env("TMUX", "/nonexistent/tmux-socket,1,0")
        .env("TMUX_PANE", "%0")
        .env(DECOY_VARIABLE, "not-a-real-secret")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success() && stdout.contains("CLI SWEEP: OK"),
        "the sweep did not pass ({}):\n{stdout}\n{stderr}",
        out.status
    );
    let after = snapshot(&machine);
    assert_eq!(
        drift(&before, &after),
        Vec::<String>::new(),
        "the sweep wrote in the machine of whoever runs it"
    );
    let left: Vec<PathBuf> = snapshot(&tmp).into_keys().collect();
    assert!(
        left.is_empty(),
        "the sweep left its scratch directory behind: {left:?}"
    );
    assert_eq!(
        proxy.requests(),
        Vec::<String>::new(),
        "something the sweep started asked for something over the network"
    );

    let noted = std::fs::read_to_string(&seen).unwrap_or_default();
    let homes: Vec<&str> = noted
        .lines()
        .filter_map(|line| line.strip_prefix("HOME="))
        .collect();
    assert!(!homes.is_empty(), "no `taw` ran under the wrapper");
    for home in &homes {
        assert!(
            Path::new(home).starts_with(&tmp),
            "a taw the sweep ran had a home outside the sweep's scratch directory: {home}"
        );
    }
    // Every tmux the sweep ran had a socket directory of the sweep's own (the caller's is in the
    // machine, which nothing wrote in), and the sweep removed it when it ended: whatever its servers
    // left there, the sweep looked, and it passed.
    let tmux_dirs: std::collections::BTreeSet<&str> = noted
        .lines()
        .filter_map(|line| line.strip_prefix("TMUX_TMPDIR="))
        .collect();
    assert_eq!(
        tmux_dirs.len(),
        1,
        "the taws the sweep ran had more than one tmux socket directory, or none: {tmux_dirs:?}"
    );
    for dir in &tmux_dirs {
        assert!(
            dir.starts_with("/tmp/taw-sweep-tmux."),
            "a taw the sweep ran had a tmux socket directory that is not the sweep's own: {dir}"
        );
        assert!(
            !Path::new(dir).exists(),
            "the sweep left its tmux socket directory behind: {dir}"
        );
    }
    let names: Vec<&str> = noted.lines().filter(|line| !line.contains('=')).collect();
    for name in [DECOY_VARIABLE, "TMUX", "TMUX_PANE"] {
        assert!(
            !names.contains(&name),
            "{name} of the caller was passed on to a taw the sweep ran"
        );
    }
    for name in [
        "HOME",
        "CLAUDE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "_ZO_DATA_DIR",
    ] {
        assert!(
            names.contains(&name),
            "{name} is not set for a taw the sweep ran, so the caller's would be found"
        );
    }
}

/// The sweep ends its server's process group (the watchers its verbs started) once the server has
/// gone, as a group alone: by then the server's pid may be another process's, which is not ended. A
/// tmux first on the sweep's `PATH` gives the pid of a lone sleep of the test's as the server's when
/// the sweep asks, and passes everything else to the real tmux (`common::tmux_giving_the_pid`). The
/// taw the sweep runs is a stand-in that answers every verb with an envelope at once, as the verdict
/// is not what this is about: the sweep is over in seconds, and its server runs no job that would be
/// left without the end of its group.
#[test]
fn the_sweep_hands_its_server_over_as_a_group_alone() {
    if which::which("python3").is_err() {
        eprintln!("skipped: the sweep needs python3 to read the envelopes");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let tmp = root.join("tmp");
    let decoy = root.join("tmux-tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&decoy).unwrap();
    let taw = root.join("bin/taw");
    common::write_script(
        &taw,
        "echo '{\"id\":\"sweep\",\"result\":{\"type\":\"stand-in\"}}'\n",
    );
    let lone = common::LoneProcess::start();
    let asked = root.join("asked.txt");
    let wrap = root.join("wrap");
    common::tmux_giving_the_pid(&wrap, lone.pid(), &asked);

    let status = Command::new("bash")
        .arg(SWEEP)
        .arg(&taw)
        .env_clear()
        .env(
            "PATH",
            format!(
                "{}:{}",
                wrap.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("TERM", "dumb")
        .env("TMPDIR", &tmp)
        .env("TMUX_TMPDIR", &decoy)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(root.join("sweep.out")).unwrap())
        .stderr(std::fs::File::create(root.join("sweep.err")).unwrap())
        .status()
        .unwrap();
    let survived = lone.alive();
    let said = std::fs::read_to_string(root.join("sweep.out")).unwrap_or_default()
        + &std::fs::read_to_string(root.join("sweep.err")).unwrap_or_default();

    assert!(
        std::fs::read_to_string(&asked).is_ok_and(|asked| !asked.is_empty()),
        "the sweep never asked its server's pid, so this proves nothing ({status}): {said}"
    );
    assert!(
        survived,
        "the sweep ended a process that has a pid its server had, which leads no group: {said}"
    );
}

/// The sweep told to stop at any moment of its start leaves neither of its directories behind: its
/// tmux socket directory (in /tmp) and its scratch directory (in `$TMPDIR`). Its traps are set before
/// either is made: a signal that came once one was made, and before they were, would end it
/// outright, with nothing to take them away. It is stopped ([`common::Stop`]) before the traps, as
/// the scratch directory is about to be made, and once both are; its taw is a stand-in that it
/// never gets to.
#[test]
fn the_sweep_told_to_stop_while_it_starts_leaves_no_directory_behind() {
    for stop_at in [
        "SOCK=\"taw-sweep-$$\"",
        "SCRATCH=\"$(mktemp",
        "CFG=\"$SCRATCH/taw-config\"",
    ] {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let tmp = root.join("tmp");
        let decoy = root.join("tmux-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::create_dir_all(&decoy).unwrap();
        let taw = root.join("bin/taw");
        common::write_script(&taw, "exit 1\n");
        let stop = common::Stop::at(root, stop_at);

        let status = Command::new("bash")
            .arg(SWEEP)
            .arg(&taw)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TERM", "dumb")
            .env("TMPDIR", &tmp)
            .env("TMUX_TMPDIR", &decoy)
            .env("BASH_ENV", &stop.bash_env)
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(root.join("sweep.out")).unwrap())
            .stderr(std::fs::File::create(root.join("sweep.err")).unwrap())
            .status()
            .unwrap();
        let noted = stop.noted().unwrap_or_default();
        let dir = noted.tmux_tmpdir.clone();
        let kept_dir = dir.starts_with("/tmp/taw-sweep-tmux.") && Path::new(&dir).exists();
        if kept_dir {
            let _ = std::fs::remove_dir_all(&dir);
        }
        let kept_scratch: Vec<PathBuf> = snapshot(&tmp).into_keys().collect();
        let said = std::fs::read_to_string(root.join("sweep.err")).unwrap_or_default();

        assert!(
            stop.noted().is_some(),
            "the sweep never got to {stop_at:?} ({status}): {said}"
        );
        assert!(
            !kept_dir,
            "the sweep, stopped at {stop_at:?}, left its tmux directory behind: {said}"
        );
        assert!(
            kept_scratch.is_empty(),
            "the sweep, stopped at {stop_at:?}, left its scratch directory behind: {kept_scratch:?}"
        );
        if stop_at.starts_with("CFG=") {
            assert!(
                dir.starts_with("/tmp/taw-sweep-tmux.") && !noted.scratch.is_empty(),
                "the sweep had made neither of its directories by {stop_at:?}, so this proves \
                 nothing: {said}"
            );
        }
    }
}
