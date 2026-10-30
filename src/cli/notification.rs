//! `taw notification …` — herdr's `notification show` and its `open_notification_target`
//! counterpart, over `notify`.

use anyhow::Context;
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::attention;
use crate::config::ToastDelivery;
use crate::machine::files;
use crate::machine::wire;
use crate::notify::{self, Class, Notification, SoundChoice};

/// herdr's `--sound`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum SoundArg {
    #[default]
    None,
    Done,
    Request,
}

impl SoundArg {
    fn choice(self) -> SoundChoice {
        match self {
            SoundArg::None => SoundChoice::None,
            SoundArg::Done => SoundChoice::Done,
            SoundArg::Request => SoundChoice::Request,
        }
    }

    /// Which lifecycle edge this notification reads as. `Notification` needs a class for
    /// its icon and colour, and `--sound` is the only signal `notification show` carries
    /// about intent: `request` means "an agent wants you", anything else "work finished".
    fn class(self) -> Class {
        match self {
            SoundArg::Request => Class::Request,
            SoundArg::None | SoundArg::Done => Class::Done,
        }
    }
}

/// herdr's `--position`. tmux's `display-message` has no position control (it draws in
/// the status line), so this is accepted and ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Position {
    #[value(name = "top-left")]
    TopLeft,
    #[value(name = "top-right")]
    TopRight,
    #[value(name = "bottom-left")]
    BottomLeft,
    #[value(name = "bottom-right")]
    BottomRight,
}

/// `--delivery` of a forwarded toast: the sending machine's `[ui.toast] delivery`. Only the two that draw where
/// the person is are forwarded (`off` shows nothing and `system` posts on the sending machine's own desktop), so
/// only these two are taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum DeliveryArg {
    Herdr,
    Terminal,
}

impl DeliveryArg {
    fn delivery(self) -> ToastDelivery {
        match self {
            DeliveryArg::Herdr => ToastDelivery::Herdr,
            DeliveryArg::Terminal => ToastDelivery::Terminal,
        }
    }
}

/// `--kind` of a forwarded toast: the words a hook's `TAW_KIND` has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Request,
    #[default]
    Done,
}

impl KindArg {
    fn class(self) -> Class {
        match self {
            KindArg::Request => Class::Request,
            KindArg::Done => Class::Done,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Show a notification
    Show {
        title: String,
        #[arg(long, value_name = "TEXT")]
        body: Option<String>,
        /// Accepted for herdr parity; tmux draws toasts in the status line.
        #[arg(long, value_enum)]
        position: Option<Position>,
        #[arg(long, value_enum, conflicts_with = "forwarded")]
        sound: Option<SoundArg>,
        /// The toast of another machine's agent, shown on this machine's clients and nothing more:
        /// no sound, no hook, and it is never sent on (what the app runs through the machine you are
        /// inside; not for typing)
        #[arg(long, value_name = "MACHINE", hide = true)]
        forwarded: Option<String>,
        /// The agent the forwarded toast is about; with none the toast is plain text, its title with
        /// the machine ahead of it (`<machine>: <title>`), except for the drop toast (`--dropped`)
        #[arg(long, value_name = "LABEL", hide = true, requires = "forwarded")]
        agent: Option<String>,
        /// The sending machine's `[ui.toast] delivery`, which draws the toast here; with none (a forward from a
        /// taw before this flag) this machine's own decides. A hidden flag added later needs the same care: a
        /// taw that does not know a flag refuses it and the toast is lost (known limit 7).
        #[arg(long, value_enum, hide = true, requires = "forwarded")]
        delivery: Option<DeliveryArg>,
        /// Where the agent is on its machine, `<session>:<window>`
        #[arg(long, value_name = "WHERE", hide = true, requires = "forwarded")]
        location: Option<String>,
        /// Whether the agent wants you or has finished
        #[arg(long, value_enum, hide = true, requires = "forwarded")]
        kind: Option<KindArg>,
        /// The forwarded toast is the one of a machine dropping off: its title already names the machine
        #[arg(long, hide = true, requires = "forwarded")]
        dropped: bool,
    },
    /// Go to the next agent: the next one that needs you after the pane you are in, else,
    /// with none that does, the next agent of any kind
    OpenTarget,
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Show {
            title,
            body,
            forwarded: Some(machine),
            agent,
            location,
            kind,
            dropped,
            delivery,
            ..
        } => {
            let toast = notify::Forwarded {
                machine,
                agent: agent.unwrap_or_default(),
                location: location.unwrap_or_default(),
                class: kind.unwrap_or_default().class(),
                title,
                body: body.unwrap_or_default(),
                dropped,
            };
            (
                "show",
                Ok(show_forwarded_as(
                    ctx,
                    &toast,
                    delivery.map(DeliveryArg::delivery),
                )),
            )
        }
        Cmd::Show {
            title, body, sound, ..
        } => (
            "show",
            Ok(show(
                ctx,
                &title,
                body.as_deref(),
                sound.unwrap_or_default(),
            )),
        ),
        Cmd::OpenTarget => ("open-target", open_target(ctx)),
    }
}

/// The test's way to show a forwarded toast: [`show_forwarded_as`] with no delivery carried.
#[cfg(test)]
fn show_forwarded(ctx: &Ctx, toast: &notify::Forwarded) -> Value {
    show_forwarded_as(ctx, toast, None)
}

/// Shows a toast another machine announced, on this machine's clients, under `delivery` when the forward
/// carried one (the sending machine's), else this machine's own: [`notify::show_forwarded_as`], which is also
/// what the app runs in its own process for a toast about another machine, while the person is inside this
/// Mac and, under delivery `system`, wherever they are (the machine it is about included). It plays no sound,
/// runs no hook and writes no events line. A failure to draw is logged and the reply is the toast's words, as
/// for [`show`].
fn show_forwarded_as(
    ctx: &Ctx,
    toast: &notify::Forwarded,
    delivery: Option<ToastDelivery>,
) -> Value {
    let delivery = delivery.unwrap_or(ctx.cfg.ui.toast.delivery);
    if let Err(err) = notify::show_forwarded_as(&ctx.tmux, delivery, toast) {
        log::warn!("forwarded toast delivery failed: {err:#}");
    }
    json!({
        "title": toast.title,
        "body": toast.body,
        "forwarded": toast.machine,
    })
}

/// Delivers a notification through the configured channels (`[ui.toast]`, `[ui.sound]`).
///
/// `pane_id` is `None`: a CLI-issued notification is not *about* a pane, so its toast is
/// the plain title and body, with no agent, no location and no jump hint.
///
/// Delivery failures (no client to draw on, no `osascript`) are logged by `deliver` and
/// do not fail the verb, so the reply is always the title and body it was given. The
/// `[hooks] notification` commands run as they do for an agent's toast, with no pane.
fn show(ctx: &Ctx, title: &str, body: Option<&str>, sound: SoundArg) -> Value {
    let notification = Notification {
        class: sound.class(),
        title: title.to_string(),
        body: body.unwrap_or_default().to_string(),
        pane_id: None,
        sound: sound.choice(),
        agent: notify::NotifiedAgent::default(),
    };
    notify::announce(&ctx.tmux, &ctx.cfg, &ctx.paths, &ctx.taw_bin, &notification);
    json!({
        "title": title,
        "body": body,
    })
}

/// `pane_id`/`target` on success (`target` is the group of the pane it went to, `"needs_you"`,
/// `"idle"` or `"rest"`: [`Group`](crate::attention::Group), serialized); a `not_found` error
/// when no pane holds an agent, which the plugin's key binding turns into `taw: no agents` on
/// the status line.
///
/// `focus: true`: this verb is what the plugin binds the notification key to, and it runs
/// from `run-shell` inside a pane, where tmux can resolve a current client to switch. The
/// popup's own `prefix+O` goes through `tui::actions` instead, which passes `false` and
/// switches its client by hand — see [`notify::open_target`].
fn open_target(ctx: &Ctx) -> anyhow::Result<Value> {
    if let Some(went) = walk_on(ctx)? {
        return Ok(went);
    }
    match notify::open_target(&ctx.tmux, true, None)? {
        Some((pane, target)) => Ok(json!({ "pane_id": pane, "target": target })),
        None => Err(CliError::not_found(
            "no agents: no pane holds an agent, so there is nowhere to go",
        )),
    }
}

/// `prefix+O` in a client the app started (the key rule: [`files::current_app_client`]): the next stop of the
/// walk the app last sent, which is every machine's. A stop of this machine is focused as ever; a stop elsewhere
/// is told to the app in a `left` line that carries it, and the client is detached, so the app goes there at once.
/// `None` for any other client, and for a walk that is stale, another app's or cannot place this pane: the caller
/// walks this machine as it always did. A detach that fails after the line was written is an error, so that the key
/// says what failed and does not walk on with the line left behind.
fn walk_on(ctx: &Ctx) -> anyhow::Result<Option<Value>> {
    let Some((client, app)) = files::current_app_client(&ctx.paths, &ctx.tmux) else {
        return Ok(None);
    };
    let Some(current) = app.pane.clone() else {
        return Ok(None);
    };
    // The walk of this app's own holder: the file of each holder is its own, so another app's walk (two Macs on one
    // machine) is never read, and an empty holder has none.
    let Some(walk) = files::read_walk(&ctx.paths, &app.holder) else {
        return Ok(None);
    };
    let (Ok(panes), Ok(sessions)) = (ctx.tmux.list_panes(), ctx.tmux.list_sessions()) else {
        return Ok(None);
    };
    let order = attention::session_order(&sessions);
    let place = |id: &str| {
        panes
            .iter()
            .find(|pane| pane.pane_id == id)
            .map(|pane| attention::position(pane, &order))
    };
    match notify::next_stop(&walk, &current, files::now_ms(), place) {
        None => Ok(None),
        Some(notify::Next::Here(pane)) => Ok(notify::focus_pane(&ctx.tmux, &panes, &pane, true)
            .map(|target| json!({ "pane_id": pane, "target": target }))),
        Some(notify::Next::Elsewhere(stop)) => {
            files::append_event(
                &ctx.paths,
                &wire::Down::Left(wire::Left {
                    holder: app.holder,
                    pane: current,
                    go: Some(stop.clone()),
                }),
            );
            ctx.tmux
                .run_ok(&["detach-client", "-t", &client])
                .with_context(|| format!("could not detach the client to go to {stop}"))?;
            Ok(Some(json!({ "went": stop })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_args_map_onto_the_notify_choices_and_classes() {
        assert_eq!(SoundArg::None.choice(), SoundChoice::None);
        assert_eq!(SoundArg::Done.choice(), SoundChoice::Done);
        assert_eq!(SoundArg::Request.choice(), SoundChoice::Request);
        assert_eq!(SoundArg::Request.class(), Class::Request);
        assert_eq!(SoundArg::Done.class(), Class::Done);
        assert_eq!(SoundArg::None.class(), Class::Done);
    }

    use crate::cli::test_support::{Stub, calls, stub_ctx, stub_tmux};

    /// A process of the test's own that lives until it is ended: the stand-in for a client, whose parent is this
    /// test (which stands in for the app child that names the client).
    struct Client(std::process::Child);

    impl Client {
        fn start() -> Client {
            Client(
                std::process::Command::new("/bin/sleep")
                    .arg("120")
                    .spawn()
                    .unwrap(),
            )
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A ctx whose client is `client`, an app that holds it (`mac-77`), and this Mac's walk file of `stops`
    /// written `age_ms` ago.
    fn with_walk(
        dir: &std::path::Path,
        client: &Client,
        stops: &[&str],
        age_ms: u64,
    ) -> (Ctx, std::path::PathBuf) {
        with_walk_of(dir, client, "mac-77", stops, age_ms)
    }

    /// [`with_walk`] with the walk file written for `walk_holder`.
    fn with_walk_of(
        dir: &std::path::Path,
        client: &Client,
        walk_holder: &str,
        stops: &[&str],
        age_ms: u64,
    ) -> (Ctx, std::path::PathBuf) {
        let (tmux, log) = stub_tmux(
            dir,
            &Stub {
                clients: true,
                client_pid: client.0.id(),
                ..Stub::default()
            },
        );
        let ctx = stub_ctx(tmux);
        files::write_app_record(
            &ctx.paths,
            &files::AppRecord {
                pid: std::process::id(),
                client_pid: client.0.id(),
                holder: "mac-77".into(),
                ..files::AppRecord::default()
            },
        );
        let stops: Vec<String> = stops.iter().map(|stop| stop.to_string()).collect();
        files::write_walk(
            &ctx.paths,
            walk_holder,
            "",
            &stops,
            files::now_ms() - age_ms,
        );
        (ctx, log)
    }

    /// In a client the app started, with the next stop on another machine, the key tells the app (a `left` line
    /// that carries the stop and names the pane the client is in), detaches that client, and answers with the stop.
    #[test]
    fn a_stop_on_another_machine_is_told_to_the_app_and_the_client_detaches() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::start();
        let (ctx, log) = with_walk(dir.path(), &client, &["%5", "pi:%3"], 1_000);

        let answer = open_target(&ctx).unwrap();

        assert_eq!(answer, json!({ "went": "pi:%3" }));
        let said = std::fs::read_to_string(ctx.paths.events_file()).unwrap();
        let line = wire::parse_down(said.lines().next().unwrap()).unwrap();
        assert_eq!(
            line,
            wire::Down::Left(wire::Left {
                holder: "mac-77".into(),
                pane: "%5".into(),
                go: Some("pi:%3".into()),
            })
        );
        assert!(
            calls(&log).contains(&vec![
                "detach-client".into(),
                "-t".into(),
                "/dev/ttys001".into()
            ]),
            "{:?}",
            calls(&log)
        );
    }

    /// A walk older than 15 s is not the app's word: the key walks this machine as it always did (nothing is
    /// told, nothing detached), and the same walk young enough does (the positive half is the test above).
    #[test]
    fn a_stale_walk_is_not_taken_and_the_client_is_not_detached() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::start();
        let (ctx, log) = with_walk(dir.path(), &client, &["%5", "pi:%3"], 16_000);

        let answer = open_target(&ctx);

        assert!(
            answer.is_err(),
            "today's path: this stand-in has no agent pane"
        );
        assert!(!ctx.paths.events_file().exists());
        assert!(
            !calls(&log).iter().any(|call| call[0] == "detach-client"),
            "{:?}",
            calls(&log)
        );
    }

    /// A client that cannot be detached after the `left` line was written is an error the key reports (the line is
    /// left behind either way, so the key must say it did not go); the positive half is the same stub with a
    /// detach that works, which answers `went`.
    #[test]
    fn a_detach_that_fails_is_the_keys_error() {
        for fails in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let client = Client::start();
            let (ctx, _) = with_walk(dir.path(), &client, &["%5", "pi:%3"], 1_000);
            if fails {
                let script = dir.path().join("tmux");
                let text = std::fs::read_to_string(&script).unwrap();
                let text = text.replace(
                    "  list-sessions)",
                    "  detach-client) exit 1 ;;\n  list-sessions)",
                );
                std::fs::write(&script, text).unwrap();
            }

            let answer = open_target(&ctx);

            if fails {
                let error = answer.unwrap_err();
                assert!(
                    format!("{error:#}").contains("could not detach the client to go to pi:%3"),
                    "{error:#}"
                );
            } else {
                assert_eq!(answer.unwrap(), json!({ "went": "pi:%3" }));
            }
        }
    }

    /// A stop of this machine is focused as today's key focuses a pane: `switch-client`, `select-window` and
    /// `select-pane` of that pane, answered with the pane and its group. The pane here (`%5`, in no agent's hands)
    /// is one that today's `open_target` cannot pick: with the walk made stale it answers not_found, so only the
    /// walk's own stop gets here.
    #[test]
    fn a_stop_of_this_machine_is_focused_and_answered() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::start();
        let (ctx, log) = with_walk(dir.path(), &client, &["%5"], 1_000);
        // The stand-in lists no panes of its own: it lists `%5`, a shell.
        let script = dir.path().join("tmux");
        let text = std::fs::read_to_string(&script).unwrap();
        let listing = crate::tmux::listing_line("%5", "title", "");
        let text = text.replace(
            "  list-panes) : ;;",
            &format!("  list-panes) printf '%s\\n' '{listing}' ;;"),
        );
        std::fs::write(&script, text).unwrap();

        let answer = open_target(&ctx).unwrap();

        assert_eq!(answer, json!({ "pane_id": "%5", "target": "rest" }));
        let verbs: Vec<Vec<String>> = calls(&log)
            .into_iter()
            .filter(|call| {
                ["switch-client", "select-window", "select-pane"].contains(&call[0].as_str())
            })
            .collect();
        let argv = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            verbs,
            [
                argv(&["switch-client", "-t", "$0"]),
                argv(&["select-window", "-t", "@0"]),
                argv(&["select-pane", "-t", "%5"]),
            ]
        );
        // Positive half: the same key with the walk gone stale is today's path, which finds no agent to go to.
        files::write_walk(
            &ctx.paths,
            "mac-77",
            "",
            &["%5".to_string()],
            files::now_ms() - 16_000,
        );
        assert!(open_target(&ctx).is_err());
    }

    /// The walk on file is the holder's of the client's app, or it is no word of that app: with two Macs on one
    /// machine the feed of each writes a file of its own, and the client of one never reads the other's. A walk of
    /// another holder gives today's path, and so does the walk of a feed that had heard no lease (an empty holder
    /// writes no file, so there is none to read); the same walk of the right holder detaches (positive half). A
    /// file that does not name its holder is the next test's.
    #[test]
    fn a_walk_of_another_holder_is_not_taken_and_the_right_holder_is() {
        for holder in ["mac-99", ""] {
            let dir = tempfile::tempdir().unwrap();
            let client = Client::start();
            let (ctx, log) = with_walk_of(dir.path(), &client, holder, &["%5", "pi:%3"], 1_000);
            assert!(open_target(&ctx).is_err(), "holder {holder:?}");
            assert!(!ctx.paths.events_file().exists(), "holder {holder:?}");
            assert!(
                !calls(&log).iter().any(|call| call[0] == "detach-client"),
                "holder {holder:?}: {:?}",
                calls(&log)
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let client = Client::start();
        let (ctx, log) = with_walk_of(dir.path(), &client, "mac-77", &["%5", "pi:%3"], 1_000);
        assert_eq!(open_target(&ctx).unwrap(), json!({ "went": "pi:%3" }));
        assert!(calls(&log).iter().any(|call| call[0] == "detach-client"));
    }

    /// A walk file that does not name the holder whose file it is (one written before the field, or by hand, or a
    /// file of another holder put under this one's name) is no word of this app: today's path.
    #[test]
    fn a_walk_file_that_does_not_name_its_holder_gives_todays_path() {
        for text in [
            r#"{"you":"","stops":["%5","pi:%3"],"at":AT}"#,
            r#"{"you":"","stops":["%5","pi:%3"],"at":AT,"holder":"mac-99"}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let client = Client::start();
            let (ctx, log) = with_walk(dir.path(), &client, &["%5"], 1_000);
            let text = text.replace("AT", &files::now_ms().to_string());
            std::fs::write(files::walk_path(&ctx.paths, "mac-77").unwrap(), &text).unwrap();
            assert!(open_target(&ctx).is_err(), "{text}");
            assert!(
                !calls(&log).iter().any(|call| call[0] == "detach-client"),
                "{text}: {:?}",
                calls(&log)
            );
        }
    }

    /// Two apps on one machine (two Macs' feeds, or a keeper and a follower) each have their own walk file, and the
    /// client of each takes its own app's walk whichever wrote last. The app record is swapped here for the second
    /// app (one test process stands in for both app children); a single walk file would give the second app's walk to
    /// the first one's client, or none.
    #[test]
    fn each_of_two_apps_on_one_machine_takes_its_own_walk() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::start();
        let (ctx, log) = with_walk(dir.path(), &client, &["%5", "pi:%3"], 1_000);
        // The other app wrote last.
        files::write_walk(
            &ctx.paths,
            "mac-88",
            "",
            &["%5".to_string(), "studio:%9".to_string()],
            files::now_ms(),
        );
        let record = |holder: &str| {
            files::write_app_record(
                &ctx.paths,
                &files::AppRecord {
                    pid: std::process::id(),
                    client_pid: client.0.id(),
                    holder: holder.into(),
                    ..files::AppRecord::default()
                },
            )
        };
        for (holder, went) in [
            ("mac-77", "pi:%3"),
            ("mac-88", "studio:%9"),
            ("mac-77", "pi:%3"),
        ] {
            record(holder);
            assert_eq!(
                open_target(&ctx).unwrap(),
                json!({ "went": went }),
                "the client of {holder}; {:?}",
                calls(&log)
            );
        }
    }

    /// With no app record and no attached record the key runs today's tmux commands and nothing more: the same
    /// argv as `notify::open_target` run alone, so nothing was asked of tmux for the rule.
    #[test]
    fn with_no_app_and_nothing_attached_the_key_runs_todays_argv() {
        let (alone_dir, key_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (tmux, alone) = stub_tmux(alone_dir.path(), &Stub::default());
        let _ = notify::open_target(&tmux, true, None);
        let (tmux, log) = stub_tmux(key_dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);

        let _ = open_target(&ctx);

        assert!(!calls(&alone).is_empty());
        assert_eq!(calls(&log), calls(&alone));
    }

    /// The texts `display-message -c <client>` was given: the toasts drawn.
    fn drawn(log: &std::path::Path) -> Vec<String> {
        calls(log)
            .into_iter()
            .filter(|call| call[0] == "display-message" && call.get(1).is_some_and(|f| f == "-c"))
            .map(|call| call[3].clone())
            .collect()
    }

    fn studio_toast(agent: &str, body: &str) -> notify::Forwarded {
        notify::Forwarded {
            machine: "studio".into(),
            agent: agent.into(),
            location: "garden:1".into(),
            class: Class::Request,
            title: "claude blocked \u{b7} garden:1".into(),
            body: body.into(),
            dropped: false,
        }
    }

    /// A forwarded toast about an agent is `format_message`'s own text for it (two spaces before the key
    /// hint, the machine ahead of the location), with this machine's key.
    #[test]
    fn a_forwarded_toast_is_the_text_format_message_makes_with_the_machine_before_the_location() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);

        show_forwarded(&ctx, &studio_toast("claude", "Do you want to proceed?"));

        assert_eq!(
            drawn(&log),
            vec![notify::tmux_message::format_message(
                Class::Request,
                "claude",
                "studio \u{b7} garden:1",
                "Do you want to proceed?",
                "O"
            )]
        );
        assert!(
            drawn(&log)[0].contains("Do you want to proceed?  #[fg=brightblack]prefix+O next up")
        );
    }

    /// The drop toast (`dropped`) is its title alone, as `format_plain_message` makes it: the title already
    /// names the machine, so there is no second `studio: `.
    #[test]
    fn the_drop_toast_is_its_title_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);
        // Through the verb, as the far side runs it: the flag is read from the words the app sends.
        let _ = run(
            &ctx,
            Cmd::Show {
                title: "studio dropped off (last seen just now)".into(),
                body: None,
                position: None,
                sound: None,
                forwarded: Some("studio".into()),
                agent: None,
                location: None,
                kind: None,
                dropped: true,
                delivery: None,
            },
        );

        assert_eq!(
            drawn(&log),
            vec![notify::tmux_message::format_plain_message(
                "studio dropped off (last seen just now)",
                ""
            )]
        );
        assert!(!drawn(&log)[0].contains("studio: "), "{}", drawn(&log)[0]);
    }

    /// Any other toast with no agent (`taw notification show "Build finished"` on that machine) names the
    /// machine ahead of its title, on the status line as on the terminal and the desktop.
    #[test]
    fn a_forwarded_toast_with_no_agent_that_is_not_the_drop_toast_names_its_machine() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);
        let toast = notify::Forwarded {
            agent: String::new(),
            location: String::new(),
            title: "Build finished".into(),
            body: "all green".into(),
            dropped: false,
            ..studio_toast("", "")
        };

        show_forwarded(&ctx, &toast);

        assert_eq!(
            drawn(&log),
            vec![notify::tmux_message::format_plain_message(
                "studio: Build finished",
                "all green"
            )]
        );
    }

    /// `[ui.toast] delivery = "off"` shows no forwarded toast, as it shows no other (the toast of the same words
    /// is seen first with `herdr`).
    #[test]
    fn a_forwarded_toast_obeys_the_delivery_setting() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let mut ctx = stub_ctx(tmux);
        show_forwarded(&ctx, &studio_toast("claude", "hello"));
        assert_eq!(drawn(&log).len(), 1, "positive: herdr draws it");

        ctx.cfg.ui.toast.delivery = ToastDelivery::Off;
        show_forwarded(&ctx, &studio_toast("claude", "hello"));

        assert_eq!(drawn(&log).len(), 1, "off draws nothing more");
    }

    /// The words of a forwarded toast through the verb, as the far side runs them, with the sending machine's
    /// `--delivery` (or none).
    fn forward_through_the_verb(ctx: &Ctx, delivery: Option<DeliveryArg>) {
        let _ = run(
            ctx,
            Cmd::Show {
                title: "claude blocked".into(),
                body: Some("hello".into()),
                position: None,
                sound: None,
                forwarded: Some("studio".into()),
                agent: Some("claude".into()),
                location: Some("garden:1".into()),
                kind: Some(KindArg::Request),
                dropped: false,
                delivery,
            },
        );
    }

    /// The sending machine's delivery draws a forwarded toast, whatever this machine's own setting is: with
    /// `herdr` it is drawn on the status line while this machine says `off`, with `terminal` it is written to the
    /// clients' terminals (nothing is drawn on the status line), and with no flag (a forward from a taw before
    /// the flag) this machine's own setting decides, as it did. The client's tty is a file of the test's own,
    /// never a device: what the stand-in lists is opened and written to.
    #[test]
    fn a_forwarded_toast_follows_the_delivery_it_carries() {
        let drew = |own: ToastDelivery, carried: Option<DeliveryArg>| {
            let dir = tempfile::tempdir().unwrap();
            let tty = dir.path().join("client-tty");
            std::fs::write(&tty, b"").unwrap();
            let (tmux, log) = stub_tmux(
                dir.path(),
                &Stub {
                    client_tty: Some(tty.clone()),
                    ..Stub::default()
                },
            );
            let mut ctx = stub_ctx(tmux);
            ctx.cfg.ui.toast.delivery = own;
            forward_through_the_verb(&ctx, carried);
            let listed = calls(&log).iter().any(|call| call[0] == "list-clients");
            let written = std::fs::read_to_string(&tty).unwrap();
            (drawn(&log).len(), listed, written)
        };
        let osc = "\x1b]777;notify;studio: claude blocked;hello\x07\x1b]9;studio: claude blocked: hello\x07";
        assert_eq!(
            drew(ToastDelivery::Herdr, None),
            (1, true, String::new()),
            "positive: no flag, herdr here draws it"
        );
        assert_eq!(
            drew(ToastDelivery::Off, Some(DeliveryArg::Herdr)),
            (1, true, String::new()),
            "herdr from the sender, off here"
        );
        assert_eq!(
            drew(ToastDelivery::Off, Some(DeliveryArg::Terminal)),
            (0, true, osc.to_string()),
            "terminal from the sender, off here"
        );
        assert_eq!(
            drew(ToastDelivery::Herdr, Some(DeliveryArg::Terminal)),
            (0, true, osc.to_string()),
            "terminal from the sender, herdr here"
        );
        assert_eq!(
            drew(ToastDelivery::Off, None),
            (0, false, String::new()),
            "no flag: this machine's off decides"
        );
    }

    /// A forwarded toast runs no hook, plays no sound and leaves no events line: the toast is seen, a moment
    /// is given to a hook that would have been started, and then there is still none.
    #[test]
    fn a_forwarded_toast_runs_no_hook_and_writes_no_events_line() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let mut ctx = stub_ctx(tmux);
        let ran = dir.path().join("hook-ran");
        ctx.cfg.hooks = toml::from_str::<crate::config::Config>(&format!(
            "[hooks]\nnotification = {}\n",
            toml::Value::String(format!("touch '{}'", ran.display()))
        ))
        .unwrap()
        .hooks;

        let words = run(
            &ctx,
            Cmd::Show {
                title: "claude blocked".into(),
                body: Some("hello".into()),
                position: None,
                sound: None,
                forwarded: Some("studio".into()),
                agent: Some("claude".into()),
                location: Some("garden:1".into()),
                kind: Some(KindArg::Request),
                dropped: false,
                delivery: None,
            },
        );

        assert_eq!(words.1.unwrap()["forwarded"], "studio");
        assert_eq!(drawn(&log).len(), 1, "positive: the toast was drawn");
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!ran.exists(), "no hook");
        assert!(!ctx.paths.events_file().exists(), "no events line");
    }

    /// Text from another machine is data: `#(...)`, `#{...}`, quotes, `$(...)`, backticks and a long run reach
    /// tmux only as `format_message` makes them (the format characters doubled), never as they came.
    #[test]
    fn a_forwarded_toast_with_format_text_in_it_is_drawn_as_format_message_escapes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);
        let body = "#(touch /tmp/marker) #{pane_id} '\"' $(id) `id` 100%";
        let toast = notify::Forwarded {
            agent: "#(touch /tmp/agent)".into(),
            location: "#{session_name}:1".into(),
            ..studio_toast("", body)
        };

        show_forwarded(&ctx, &toast);

        let text = drawn(&log).remove(0);
        assert_eq!(
            text,
            notify::tmux_message::format_message(
                Class::Request,
                "#(touch /tmp/agent)",
                "studio \u{b7} #{session_name}:1",
                body,
                "O"
            )
        );
        assert!(text.contains("##(touch /tmp/marker) ##{pane_id}"), "{text}");
        assert!(
            !text.contains(" #(touch") && !text.contains(" #{pane_id}"),
            "{text}"
        );
    }

    /// What the wire cuts (`wire::clean_text`: control characters, escape sequences, 256 characters) is cut
    /// before a forwarded toast is made, and then `format_message` cuts the detail as it does a local toast:
    /// the forwarded toast of a long dirty body is the local one's text for the same cleaned body.
    #[test]
    fn a_forwarded_toast_is_cut_as_a_local_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);
        let dirty = format!("a\u{1b}[2Jb\u{7}c{}", "x".repeat(400));
        let cleaned = wire::clean_text(&dirty);
        assert_eq!(cleaned.chars().count(), crate::machine::clean::MAX_TEXT);
        assert!(
            cleaned.starts_with("abcxxx"),
            "the sequence and the bell are gone"
        );

        show_forwarded(&ctx, &studio_toast("claude", &cleaned));

        let text = drawn(&log).remove(0);
        assert_eq!(
            text,
            notify::tmux_message::format_message(
                Class::Request,
                "claude",
                "studio \u{b7} garden:1",
                &cleaned,
                "O"
            )
        );
        assert!(
            text.contains(&format!("abc{}\u{2026}", "x".repeat(57))),
            "the detail is cut at 60 characters like a local toast's: {text}"
        );
    }
}
