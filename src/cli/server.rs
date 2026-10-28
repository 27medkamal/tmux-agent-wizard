//! `taw server started`: what the plugin runs, in the background, each time it loads on a tmux server.
//! The work is in [`crate::crash`]; this is the part that reads the server, and reaches the screen
//! and the clock.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Subcommand;
use serde_json::{Value, json};

use super::{Ctx, Sub};
use crate::config::ToastDelivery;
use crate::crash::{self, Effects, Search, Seen, Server};
use crate::notify::{self, Notification};

/// Where macOS puts crash reports, under the home directory. `$TAW_CRASH_REPORTS_DIR` names another
/// folder, for a test.
const REPORTS_UNDER_HOME: &str = "Library/Logs/DiagnosticReports";
/// How long to look for a crash report, in seconds (fractions allowed), for a test.
const WATCH_ENV: &str = "TAW_CRASH_WATCH_SECS";
/// How long to wait for a client to be attached for a toast that needs one, in seconds, for a test.
const CLIENT_WAIT_ENV: &str = "TAW_CRASH_CLIENT_WAIT_SECS";
/// How long to wait between two looks, in milliseconds, for a test.
const POLL_ENV: &str = "TAW_CRASH_POLL_MS";
const REPORTS_ENV: &str = "TAW_CRASH_REPORTS_DIR";

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Note that a tmux server is up, and keep the crash report of the server before it
    ///
    /// The plugin runs this in the background each time it loads, and its output is discarded.
    /// It writes down which server this is (its pid and start time, in the state directory under
    /// servers). When the last server on the same socket is gone, on macOS, it looks for up to
    /// two minutes in ~/Library/Logs/DiagnosticReports for the crash report macOS wrote for it
    /// if it crashed, copies it into the state directory under tmux-crashes, and tells you once
    /// where it is. A server that was killed, or ended with its last session, leaves no report
    /// and nothing is said. Loading the plugin again on the same server does nothing.
    Started,
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Started => ("started", started(ctx)),
    }
}

/// A number of seconds from the environment, for a test; `None` when it is unset or not a number.
fn seconds_from_env(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(Duration::from_secs_f64)
}

/// The server this runs against: its socket, pid and start time.
fn read_server(ctx: &Ctx) -> anyhow::Result<Server> {
    let out = ctx
        .tmux
        .run(&[
            "display-message",
            "-p",
            "#{socket_path}\t#{pid}\t#{start_time}",
        ])
        .context("asking tmux which server this is")?;
    let mut fields = out.rsplitn(3, '\t');
    let start_time = fields.next().and_then(|f| f.trim().parse().ok());
    let pid = fields.next().and_then(|f| f.trim().parse().ok());
    let socket_path = fields.next().filter(|path| !path.is_empty());
    match (socket_path, pid, start_time) {
        (Some(socket_path), Some(pid), Some(start_time)) => Ok(Server {
            socket_path: socket_path.to_string(),
            pid,
            start_time,
        }),
        _ => anyhow::bail!("tmux did not say which server this is: {out:?}"),
    }
}

/// The real places, clock and screen.
struct Real<'a> {
    ctx: &'a Ctx,
}

impl Effects for Real<'_> {
    fn sleep(&mut self, how_long: Duration) {
        std::thread::sleep(how_long);
    }

    fn client_attached(&mut self) -> bool {
        self.ctx
            .tmux
            .list_clients()
            .is_ok_and(|clients| !clients.is_empty())
    }

    fn announce(&mut self, notification: &Notification) {
        notify::announce(
            &self.ctx.tmux,
            &self.ctx.cfg,
            &self.ctx.paths,
            &self.ctx.taw_bin,
            notification,
        );
    }

    fn utc_offset(&self, unix: i64) -> i64 {
        u64::try_from(unix).map_or(0, crate::session::restore::local_offset)
    }
}

fn started(ctx: &Ctx) -> anyhow::Result<Value> {
    let server = read_server(ctx)?;
    let seen = crash::note_server(&ctx.paths.state_dir.join(crash::SERVERS_DIR), &server)
        .context("writing down this server")?;
    let mut found = crash::Found::default();
    if let Seen::Replaced { last } = &seen
        && cfg!(target_os = "macos")
        && !crate::watch::process_alive(last.pid)
    {
        let home = crate::paths::home_dir();
        let reports_dir = std::env::var_os(REPORTS_ENV)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(REPORTS_UNDER_HOME));
        let crashes_dir = ctx.paths.state_dir.join(crash::CRASHES_DIR);
        let search = Search {
            reports_dir: &reports_dir,
            crashes_dir: &crashes_dir,
            home: Some(&home),
            watch: seconds_from_env(WATCH_ENV).unwrap_or(crash::WATCH_FOR),
            poll: std::env::var(POLL_ENV)
                .ok()
                .and_then(|ms| ms.trim().parse::<u64>().ok())
                .map_or(crash::POLL_EVERY, Duration::from_millis),
            client_wait: seconds_from_env(CLIENT_WAIT_ENV).unwrap_or(crash::CLIENT_WAIT),
            needs_client: matches!(
                ctx.cfg.ui.toast.delivery,
                ToastDelivery::Herdr | ToastDelivery::Terminal
            ),
        };
        found = crash::keep_report_of(last, &search, &mut Real { ctx });
    }
    Ok(json!({
        "socket_path": server.socket_path,
        "pid": server.pid,
        "start_time": server.start_time,
        "record": match seen {
            Seen::Reload => "unchanged",
            Seen::First | Seen::Replaced { .. } => "written",
        },
        "crash_report": found.kept.map(|path| path.display().to_string()),
        "announced": found.announced,
    }))
}
