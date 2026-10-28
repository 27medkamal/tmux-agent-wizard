//! `[ui.sound]`: the notification sound that accompanies a toast (any delivery mode,
//! `off` included — herdr's `off` only silences the pop-up, not the sound).

use crate::config::{Sound, SoundMode};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::SoundChoice;

/// Plays the sound for `choice`, subject to gating, then returns immediately — the
/// player process is never waited on.
///
/// Gating, in order: `cfg.enabled == false` mutes everything (`Ok`, nothing spawned);
/// `cfg.agents.get(agent) == Some(SoundMode::Off)` mutes that one agent (`Ok`, nothing
/// spawned — herdr's default map sets `droid = off`); otherwise the path is chosen as
/// the first present of `request_path`/`path` (for [`SoundChoice::Request`]),
/// `done_path`/`path` (for [`SoundChoice::Done`]), or bare `path` (for
/// [`SoundChoice::None`] — there is no dedicated "none" sound). No path configured is
/// `Ok`, nothing spawned — this crate ships no bundled audio. A relative path is
/// resolved against `config_dir` (the directory `config.toml` itself lives in, same as
/// herdr); an absolute path is used as-is.
///
/// The player is `afplay` on macOS, `paplay` (falling back to `aplay` when `paplay`
/// is not on `$PATH`) on Linux, spawned detached with stdio nulled.
pub fn play(
    cfg: &Sound,
    config_dir: &Path,
    agent: &str,
    choice: SoundChoice,
) -> anyhow::Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    if cfg.agents.get(agent).copied().unwrap_or_default() == SoundMode::Off {
        return Ok(());
    }
    let Some(path) = choose_path(cfg, choice) else {
        return Ok(());
    };
    let resolved = resolve_path(config_dir, path);
    spawn_player(&resolved)
}

fn choose_path(cfg: &Sound, choice: SoundChoice) -> Option<&PathBuf> {
    match choice {
        SoundChoice::Request => cfg.request_path.as_ref().or(cfg.path.as_ref()),
        SoundChoice::Done => cfg.done_path.as_ref().or(cfg.path.as_ref()),
        SoundChoice::None => cfg.path.as_ref(),
    }
}

fn resolve_path(config_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

/// Test seam: when set, `play` spawns `$TAW_SOUND_CMD <path>` instead of the real
/// platform player (mirrors `system::send`'s `$TAW_NOTIFY_CMD`). Lets tests observe
/// that a sound was genuinely spawned — and is reaped rather than left a zombie —
/// without touching real audio hardware or depending on `afplay`/`paplay`/`aplay`
/// being installed on whatever machine runs the tests.
const TEST_CMD_ENV: &str = "TAW_SOUND_CMD";

/// The player `play` runs on macOS, and the two it runs on Linux. The tests' desktop net
/// (scripts/desktop-net-names.txt) has a refusing stub of each, which a unit test holds it to.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const AFPLAY: &str = "afplay";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) const PAPLAY: &str = "paplay";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) const APLAY: &str = "aplay";

fn player_command(path: &Path) -> anyhow::Result<Command> {
    if let Ok(cmd) = std::env::var(TEST_CMD_ENV) {
        let mut c = Command::new(cmd);
        c.arg(path);
        return Ok(c);
    }
    native_player_command(path)
}

#[cfg(target_os = "macos")]
fn native_player_command(path: &Path) -> anyhow::Result<Command> {
    let mut cmd = Command::new(AFPLAY);
    cmd.arg(path);
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn native_player_command(path: &Path) -> anyhow::Result<Command> {
    let bin = if which::which(PAPLAY).is_ok() {
        PAPLAY
    } else {
        APLAY
    };
    let mut cmd = Command::new(bin);
    cmd.arg(path);
    Ok(cmd)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_player_command(_path: &Path) -> anyhow::Result<Command> {
    anyhow::bail!("no sound player is available on this platform")
}

/// Spawns the player detached (nulled stdio) and returns immediately — the caller
/// never waits on it. The `Child` handle is moved onto its own thread that calls
/// `wait()` and discards the result: dropping a `Child` without waiting leaves a
/// `<defunct>` zombie process behind until *this* process itself exits, which matters
/// for a long-lived watcher process that can call this many times over its lifetime.
/// The reaper thread does not block `spawn_player`'s caller — only the spawned
/// player's own exit blocks that one background thread, never the notification
/// pipeline.
fn spawn_player(path: &Path) -> anyhow::Result<()> {
    let mut cmd = player_command(path)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(enabled: bool) -> Sound {
        Sound {
            enabled,
            path: Some(PathBuf::from("notify.wav")),
            done_path: Some(PathBuf::from("done.wav")),
            request_path: Some(PathBuf::from("request.wav")),
            agents: BTreeMap::new(),
        }
    }

    #[test]
    fn disabled_globally_returns_ok_without_choosing_a_path_or_spawning() {
        let cfg = cfg(false);
        // A bogus config_dir proves nothing was ever opened/spawned: if `play` tried
        // to resolve or play anything, a real player binary would still just no-op on
        // a missing file, so the real assertion is simply that this returns `Ok` at
        // all despite `enabled: false` — see the doc comment's gating order.
        assert!(
            play(
                &cfg,
                Path::new("/nonexistent"),
                "claude",
                SoundChoice::Request
            )
            .is_ok()
        );
        assert!(play(&cfg, Path::new("/nonexistent"), "claude", SoundChoice::Done).is_ok());
    }

    #[test]
    fn per_agent_off_returns_ok_without_spawning() {
        let mut cfg = cfg(true);
        cfg.agents.insert("droid".to_string(), SoundMode::Off);

        assert!(
            play(
                &cfg,
                Path::new("/nonexistent"),
                "droid",
                SoundChoice::Request
            )
            .is_ok()
        );
    }

    #[test]
    fn no_path_configured_returns_ok_without_spawning() {
        let cfg = Sound {
            enabled: true,
            path: None,
            done_path: None,
            request_path: None,
            agents: BTreeMap::new(),
        };

        assert!(
            play(
                &cfg,
                Path::new("/nonexistent"),
                "claude",
                SoundChoice::Request
            )
            .is_ok()
        );
        assert!(play(&cfg, Path::new("/nonexistent"), "claude", SoundChoice::Done).is_ok());
        assert!(play(&cfg, Path::new("/nonexistent"), "claude", SoundChoice::None).is_ok());
    }

    #[test]
    fn choose_path_prefers_the_choice_specific_path_falling_back_to_the_generic_one() {
        let full = cfg(true);
        assert_eq!(
            choose_path(&full, SoundChoice::Request),
            Some(&PathBuf::from("request.wav"))
        );
        assert_eq!(
            choose_path(&full, SoundChoice::Done),
            Some(&PathBuf::from("done.wav"))
        );
        assert_eq!(
            choose_path(&full, SoundChoice::None),
            Some(&PathBuf::from("notify.wav"))
        );

        let generic_only = Sound {
            enabled: true,
            path: Some(PathBuf::from("notify.wav")),
            done_path: None,
            request_path: None,
            agents: BTreeMap::new(),
        };
        assert_eq!(
            choose_path(&generic_only, SoundChoice::Request),
            Some(&PathBuf::from("notify.wav"))
        );
        assert_eq!(
            choose_path(&generic_only, SoundChoice::Done),
            Some(&PathBuf::from("notify.wav"))
        );
    }

    #[test]
    fn relative_paths_resolve_against_config_dir_absolute_paths_pass_through() {
        assert_eq!(
            resolve_path(Path::new("/cfg"), Path::new("request.wav")),
            PathBuf::from("/cfg/request.wav")
        );
        assert_eq!(
            resolve_path(Path::new("/cfg"), Path::new("/abs/sound.wav")),
            PathBuf::from("/abs/sound.wav")
        );
    }

    // `play` reads a process-global env var when the test seam is used; serialize
    // the tests that set it so they cannot interleave under the default
    // multi-threaded test runner (mirrors `notify::system`'s `ENV_LOCK` /
    // `paths.rs`'s).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
    }

    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: single-threaded within the lock; no other code in this process
            // reads/writes this var.
            unsafe {
                std::env::remove_var(TEST_CMD_ENV);
            }
        }
    }

    fn with_test_cmd(script: &Path) -> EnvGuard<'static> {
        let guard = EnvGuard {
            _lock: ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
        };
        // SAFETY: serialized by ENV_LOCK above.
        unsafe {
            std::env::set_var(TEST_CMD_ENV, script);
        }
        guard
    }

    /// Polls `ready` until it answers `true` or `timeout` elapses. Nothing in this module
    /// sleeps for a fixed duration waiting on the player: a spawned child settles in its
    /// own time, and a poll with a ceiling is both faster and not load-sensitive.
    fn wait_until(mut ready: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if ready() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn test_cmd_seam_spawns_with_the_resolved_path_without_blocking_the_caller_and_is_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let recorded = tmp.path().join("recorded-arg.txt");
        let staging = tmp.path().join("recorded-arg.partial");
        let gate = tmp.path().join("gate");
        let script = tmp.path().join("record.sh");

        // The player is *gated*, not slept: it blocks until this test creates `gate`, so
        // "play() returned while the child was still running" is an observable fact rather
        // than a race against a fixed sleep. It then writes its marker to a staging path
        // and renames it into place — an atomic rename on one filesystem, so a reader can
        // never catch a half-written (or freshly truncated, still empty) marker.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 i=0\n\
                 while [ ! -e '{gate}' ] && [ $i -lt 2000 ]; do sleep 0.01; i=$((i+1)); done\n\
                 printf '%s' \"$1\" > '{staging}'\n\
                 mv '{staging}' '{recorded}'\n",
                gate = gate.display(),
                staging = staging.display(),
                recorded = recorded.display(),
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);

        let _guard = with_test_cmd(&script);
        let mut cfg = cfg(true);
        cfg.request_path = Some(PathBuf::from("beep.wav"));

        let started = std::time::Instant::now();
        play(&cfg, tmp.path(), "claude", SoundChoice::Request).unwrap();
        let elapsed = started.elapsed();

        // The child cannot finish before the gate exists, so `play()` waiting on it would
        // hang for the script's whole 20 s ceiling. Returning well inside that is the
        // proof that it did not wait.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "play() must return without waiting on the player: took {elapsed:?}"
        );
        assert!(
            !recorded.exists(),
            "the player is still gated, so it cannot have written its marker yet"
        );

        // The detached child still runs to completion and is reaped (not left a zombie) by
        // spawn_player's background wait() thread, not by this test.
        std::fs::write(&gate, "").unwrap();
        let expected = tmp.path().join("beep.wav").to_string_lossy().into_owned();
        assert!(
            wait_until(
                || std::fs::read_to_string(&recorded).is_ok_and(|got| got == expected),
                std::time::Duration::from_secs(30),
            ),
            "the detached, reaped child should still finish and record {expected:?}, got {:?}",
            std::fs::read_to_string(&recorded),
        );
    }
}
