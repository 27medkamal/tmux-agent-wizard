// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! `[ui.toast] delivery = "system"`: the host OS's own notification centre.

use anyhow::Context;
use std::process::Command;

/// Test seam: when set, `send` runs `$TAW_NOTIFY_CMD <title> <body>` (two plain argv
/// entries, no shell involved) instead of touching the real OS notifier, and returns
/// based on that command's exit status. Tests point this at a small recording script
/// so they can assert on delivery without a real macOS/Linux notification popping up.
const TEST_CMD_ENV: &str = "TAW_NOTIFY_CMD";

/// The notifier this delivery runs on macOS, and the one it runs on Linux. The tests' desktop net
/// (scripts/desktop-net-names.txt) has a refusing stub of each, which a unit test holds it to.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const OSASCRIPT: &str = "osascript";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) const NOTIFY_SEND: &str = "notify-send";

/// Sends an OS-native notification. macOS: `osascript -e 'display notification "<body>"
/// with title "<title>"'` (`"` and `\` in `title`/`body` are backslash-escaped so the
/// generated AppleScript string literals stay well-formed). Linux: `notify-send --app-name
/// tmux-agent-wizard -- <title> [<body>]` (see [`notify_send_args`]). Any other platform:
/// `Err` — there is no supported native notifier (documented in the parity table).
/// `$TAW_NOTIFY_CMD`, when set, pre-empts all of the above (see [`TEST_CMD_ENV`]).
pub fn send(title: &str, body: &str) -> anyhow::Result<()> {
    if let Ok(cmd) = std::env::var(TEST_CMD_ENV) {
        return run(&cmd, &[title, body]);
    }
    send_native(title, body)
}

#[cfg(target_os = "macos")]
fn send_native(title: &str, body: &str) -> anyhow::Result<()> {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(body),
        escape_applescript(title)
    );
    run(OSASCRIPT, &["-e", &script])
}

#[cfg(target_os = "macos")]
fn escape_applescript(s: &str) -> String {
    // Backslashes first, so the backslash a quote-escape introduces is not itself
    // re-escaped by a second pass over the same characters.
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn send_native(title: &str, body: &str) -> anyhow::Result<()> {
    run(NOTIFY_SEND, &notify_send_args(title, body))
}

/// The name Linux notification centres show as a notification's source.
#[cfg(any(target_os = "linux", test))]
const NOTIFY_APP_NAME: &str = "tmux-agent-wizard";

// Follows herdr's src/platform/linux.rs. Differences: the app name is
// taw's own, and herdr's $DISPLAY/$WAYLAND_DISPLAY pre-check is not ported.
/// `notify-send`'s arguments: `--app-name` makes the notification identify taw as its
/// source rather than `notify-send`, and `--` ends option parsing so a title beginning
/// with `-` is shown instead of being read as an option. An empty body is left off.
///
/// Follows herdr's `platform/linux.rs` `show_desktop_notification_with_command`
/// (#3638), which passes `--app-name Herdr --`; the app name is taw's own.
#[cfg(any(target_os = "linux", test))]
fn notify_send_args<'a>(title: &'a str, body: &'a str) -> Vec<&'a str> {
    let mut args = vec!["--app-name", NOTIFY_APP_NAME, "--", title];
    if !body.is_empty() {
        args.push(body);
    }
    args
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn send_native(_title: &str, _body: &str) -> anyhow::Result<()> {
    anyhow::bail!("`system` toast delivery has no native notifier on this platform")
}

fn run(bin: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("running `{bin}`"))?;
    anyhow::ensure!(status.success(), "`{bin}` exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `send` reads a process-global env var; serialize the tests that set it so they
    // cannot interleave under the default multi-threaded test runner (mirrors
    // `paths.rs`'s `ENV_LOCK`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    fn with_test_cmd(script: &std::path::Path) -> EnvGuard<'static> {
        let guard = EnvGuard {
            _lock: ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
        };
        // SAFETY: serialized by ENV_LOCK above.
        unsafe {
            std::env::set_var(TEST_CMD_ENV, script);
        }
        guard
    }

    #[test]
    fn test_cmd_seam_runs_the_configured_command_with_title_and_body_as_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let recorded = tmp.path().join("recorded.txt");
        let script = tmp.path().join("record.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n' \"$1\" \"$2\" > {}\n",
                recorded.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);

        let _guard = with_test_cmd(&script);
        send("a title", "a body").unwrap();

        let contents = std::fs::read_to_string(&recorded).unwrap();
        assert_eq!(contents, "a title\na body\n");
    }

    /// herdr v0.9.1 (#3638), `platform/linux.rs`: the notification names its source, and a
    /// title that starts with `-` is shown as the title instead of being read as an option.
    #[test]
    fn notify_send_names_taw_and_ends_the_options_before_the_title() {
        assert_eq!(
            notify_send_args("-danger", "body"),
            ["--app-name", "tmux-agent-wizard", "--", "-danger", "body"]
        );
        assert_eq!(
            notify_send_args("claude done", ""),
            ["--app-name", "tmux-agent-wizard", "--", "claude done"],
            "an empty body is left off, as herdr leaves it"
        );
    }

    /// The same argv, through a real process: what the child receives is exactly the argv
    /// above, so a title beginning with `-` reaches it as an operand.
    #[test]
    fn notify_send_args_reach_a_child_process_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let recorded = tmp.path().join("args.txt");
        let script = tmp.path().join("notify-send");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                recorded.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);

        run(
            script.to_str().unwrap(),
            &notify_send_args("-danger", "a body"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&recorded).unwrap(),
            "--app-name\ntmux-agent-wizard\n--\n-danger\na body\n"
        );
    }

    #[test]
    fn test_cmd_seam_propagates_a_nonzero_exit_as_err() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fail.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        crate::test_exec::make_executable(&script);

        let _guard = with_test_cmd(&script);
        assert!(send("t", "b").is_err());
    }
}
