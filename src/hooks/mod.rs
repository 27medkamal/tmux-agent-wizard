//! `[hooks]`: the user's own commands, run on taw's lifecycle events and on every
//! notification taw shows, plus two hooks that can refuse a close or a worktree removal.
//! See "Hooks" in the README for what a person sees; this module is what runs them.
//!
//! [`fire`] runs the commands of an event in the background. [`pre_close`] and
//! [`pre_worktree_remove`] run theirs to the end, and a failure is a [`create::HookFailure`],
//! which is how the close or the removal is refused.

pub mod context;
pub mod events;
pub mod log;
pub mod run;
pub mod sample;
pub mod server_env;

use anyhow::Context as _;
use serde_json::{Value, json};

use self::context::Built;
use self::server_env::{ServerEnv, SessionEnv};
use crate::config::Config;
use crate::notify::{self, Notification};
use crate::plugin::{self, PluginCtx};
use crate::session::create::{self, Hook, HookEnv, Streams};
use crate::session::worktree::WorktreeRecord;
use crate::tmux::{PaneInfo, Tmux, ids};

/// Starts the `[hooks]` commands hooked to `event`, each in the background, with what `payload`
/// says about the event, and returns how many started. This is what every place that fires an
/// event calls, through `plugin::events::emit`, and it never waits for a hook: the caller may
/// be the popup, the watcher or a `taw` verb.
///
/// `pane.agent_status_changed` is also the `agent.<state>` event for the state the agent went
/// into (see [`events::agent_event`]): the same moment, hooked by what a person cares about,
/// whose command is told it is the agent event.
///
/// When nothing is hooked to the event this does nothing at all, tmux included.
pub fn fire(ctx: &PluginCtx, event: &str, payload: &Value) -> usize {
    fire_with(ctx, event, payload, &mut None)
}

/// [`fire`], with the pane list the caller has already read, if it has, and leaving there the
/// one it reads if it has not.
fn fire_with(
    ctx: &PluginCtx,
    event: &str,
    payload: &Value,
    panes: &mut Option<Vec<PaneInfo>>,
) -> usize {
    let agent_event = (event == events::STATUS_CHANGED)
        .then(|| events::agent_event(payload))
        .flatten();
    let mut started = 0;
    for name in std::iter::once(event).chain(agent_event) {
        if ctx.cfg.hooks.commands(name).is_empty() {
            continue;
        }
        // What the payload leaves out (a session's name, a window's directory) is read from
        // tmux once, and only because a hook wants it. A tmux that cannot answer leaves it
        // empty.
        let panes = panes.get_or_insert_with(|| ctx.tmux.list_panes().unwrap_or_default());
        started += run::dispatch(ctx, name, &context::build(name, payload, panes));
    }
    started
}

/// The environment a hook is started with, on top of the one its process has: what tmux would
/// give a new pane in the session the event is about ([`server_env`]), which is tmux's global
/// environment with that session's own over it, and `TMUX` for the server, then `taw`, the
/// `TAW_*` variables of the event, which win. Every hook gets it, whichever process starts the
/// hook, so a hook that needs a credential, or the ssh agent of the terminal its session was
/// attached from, finds it in the same place whether the watcher, the popup, a pane or a tmux
/// hook fired it.
///
/// A caller that keeps tmux's global environment (`ctx.server_env`, the watcher) has it read once.
/// Any other reads it now. The session's own environment is read each time, since it changes
/// whenever a client attaches. A tmux that cannot answer leaves the hook with `taw` alone, on top
/// of what its process has, and an event about no session, or one that is gone, has the global
/// environment alone.
pub fn environment(ctx: &PluginCtx, taw: &[(String, String)]) -> HookEnv {
    let server = match ctx.server_env {
        Some(cache) => cache.get(ctx.tmux),
        None => match ServerEnv::read(ctx.tmux) {
            Ok(server) => Some(std::sync::Arc::new(server)),
            Err(err) => {
                ::log::debug!("could not read tmux's global environment for a hook: {err:#}");
                None
            }
        },
    };
    match server {
        Some(server) => {
            let session = server_env::session_of(taw).and_then(|id| SessionEnv::read(ctx.tmux, id));
            server.for_hook(session.as_ref(), taw)
        }
        None => HookEnv {
            set: taw.to_vec(),
            unset: Vec::new(),
        },
    }
}

/// Whether any `[hooks]` command is hooked to `event`. The status event counts when only an
/// `agent.*` event is hooked, since that is the event it is emitted as.
pub fn any_hook(cfg: &Config, event: &str) -> bool {
    let hooked = |name: &str| !cfg.hooks.commands(name).is_empty();
    hooked(event)
        || (event == events::STATUS_CHANGED && events::AGENT_EVENTS.iter().any(|name| hooked(name)))
}

/// Runs the `[hooks] notification` commands for a notification taw is showing, and returns
/// how many started. The payload is what the toast says: `kind` (`request` or `done`), the
/// plain `message` ([`crate::notify::plain_text`]), its `title` and `body` as they were
/// decided, and, for a notification about a pane, that pane, its agent and the state it is in.
/// The agent's state is the one the notification is for when the pane no longer holds an agent
/// (one that exited mid-turn is notified after its record is cleared).
///
/// Called where the toast is delivered ([`crate::notify::announce`]), so it fires exactly when
/// a toast would: after the same delay and never while you are looking at that pane.
pub fn notification(ctx: &PluginCtx, n: &Notification) -> usize {
    notification_from(ctx, n, None)
}

/// [`notification`] for a notification that may be about another machine: `machine` is that machine's
/// name in `[machines]` and `TAW_MACHINE`, and the JSON's `machine`, say so. `Some("")` is this Mac's own,
/// told through the app (`TAW_MACHINE` empty, `machine` empty); `None` is a notification no app told, which
/// has no `machine` in its JSON.
///
/// A notification about a machine is about no pane of this one: its pane is that machine's (the notification
/// names it as `studio:%31`), so this Mac's panes are not looked at, and a pane of this Mac with the same
/// number is not taken for it. The pane fields stay empty, and the JSON's `pane_id` is the machine's own,
/// namespaced as the app names it.
pub fn notification_from(ctx: &PluginCtx, n: &Notification, machine: Option<&str>) -> usize {
    if ctx.cfg.hooks.commands(events::NOTIFICATION).is_empty() {
        return 0;
    }
    let remote = machine.is_some_and(|name| !name.is_empty());
    let mut panes = Some(if remote {
        Vec::new()
    } else {
        ctx.tmux.list_panes().unwrap_or_default()
    });
    let mut payload = json!({
        "kind": notify::kind_word(n.class),
        "message": notify::plain_text(n),
        "title": n.title,
        "body": n.body,
    });
    if let Some(name) = machine {
        payload["machine"] = json!(name);
    }
    if let Some(id) = &n.pane_id {
        let pane = panes.iter().flatten().find(|pane| pane.pane_id == *id);
        let agent = pane
            .and_then(|pane| pane.agent.kind.clone())
            .or_else(|| Some(n.agent.kind.clone()).filter(|kind| !kind.is_empty()));
        let status = pane
            .filter(|pane| pane.agent.kind.is_some())
            .and_then(|pane| pane.agent.state)
            .map_or(
                match n.class {
                    notify::Class::Request => "blocked",
                    notify::Class::Done => "done",
                },
                |state| state.as_str(),
            );
        payload["agent"] = json!(agent);
        payload["agent_status"] = json!(status);
        if let Some(pane) = pane {
            payload["pane_id"] = json!(ids::pane_id(&pane.session_id, &pane.pane_id));
            payload["workspace_id"] = json!(ids::workspace_id(&pane.session_id));
            payload["pane"] = crate::plugin::context::pane_json(pane);
        } else if let Some(name) = machine.filter(|_| remote) {
            // That machine's pane, in the app's names: it is no id of this one's (`TAW_PANE` stays empty).
            payload["pane_id"] = json!(match crate::machine::split_id(id) {
                (Some(_), _) => id.clone(),
                (None, raw) => format!("{name}:{raw}"),
            });
        }
    }
    fire_with(ctx, events::NOTIFICATION, &payload, &mut panes)
}

/// What a close is about, by tmux's own id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closing<'a> {
    /// A session, `$3`.
    Session(&'a str),
    /// A window, `@7`.
    Window(&'a str),
    /// A pane, `%31`.
    Pane(&'a str),
}

/// Asks the `[hooks] pre_close` commands whether `closing` may go ahead. `Ok` when there are
/// none, or every one exited zero; a [`create::HookFailure`] as soon as one exits non-zero or runs past
/// `[sessions] hook_timeout_seconds`, which the caller turns into a refusal: nothing is closed,
/// the popup shows [`create::HookFailure::summary`], and a CLI verb fails with `hook_refused`.
///
/// This runs where the close is asked for, before tmux is told anything: the popup's `x`, and
/// `workspace close`, `tab close` and `pane close`. A hook taw waits for gets the same
/// environment and JSON as an event hook, with `TAW_TARGET` saying whether it is a `session`,
/// a `window` or a `pane` (the JSON says the same in `target`), and starts in that thing's
/// directory. It has the create hooks' time limit, not `[hooks] timeout_seconds`, because a
/// person is waiting for it.
///
/// It cannot see a close taw is not asked about: `prefix x` and `prefix &` are tmux's own, and
/// so is a process that ends. Two closes taw makes are not asked about either: `taw worktree
/// remove` kills the worktree's session when it is open, which `pre_worktree_remove` covers,
/// and `taw plugin pane close` kills a pane a plugin opened.
pub fn pre_close(ctx: &PluginCtx, closing: Closing<'_>) -> anyhow::Result<()> {
    if ctx.cfg.hooks.commands(events::PRE_CLOSE).is_empty() {
        return Ok(());
    }
    let panes = ctx.tmux.list_panes().unwrap_or_default();
    let payload = closing_payload(ctx.tmux, closing, &panes);
    let built = context::build(events::PRE_CLOSE, &payload, &panes);
    veto(ctx, Hook::PreClose, events::PRE_CLOSE, &built)
}

/// Asks the `[hooks] pre_worktree_remove` commands whether a worktree may be removed, as
/// [`pre_close`] asks about a close. The hook is told the worktree the way `worktree.removed`
/// tells a plugin: its record, with `TAW_DIR` its checkout and `TAW_SESSION` the session it
/// has or had. Asked before git is: a worktree that is refused is untouched.
pub fn pre_worktree_remove(ctx: &PluginCtx, record: &WorktreeRecord) -> anyhow::Result<()> {
    if ctx
        .cfg
        .hooks
        .commands(events::PRE_WORKTREE_REMOVE)
        .is_empty()
    {
        return Ok(());
    }
    let panes = ctx.tmux.list_panes().unwrap_or_default();
    let payload = json!({ "workspace_id": record.workspace_id, "worktree": record });
    let built = context::build(events::PRE_WORKTREE_REMOVE, &payload, &panes);
    veto(
        ctx,
        Hook::PreWorktreeRemove,
        events::PRE_WORKTREE_REMOVE,
        &built,
    )
}

/// What a close is about, as an event payload: the thing's record when tmux can give it
/// (`workspace`, `tab` or `pane`, the shapes a plugin gets) and its id when it cannot, and
/// `target` either way.
fn closing_payload(t: &Tmux, closing: Closing<'_>, panes: &[PaneInfo]) -> Value {
    let (target, id_key, id) = match closing {
        Closing::Session(id) => ("session", "workspace_id", id),
        Closing::Window(id) => ("window", "tab_id", id),
        Closing::Pane(id) => ("pane", "pane_id", id),
    };
    let record = match closing {
        Closing::Session(id) => t.list_sessions().ok().and_then(|sessions| {
            let session = sessions.into_iter().find(|s| s.session_id == id)?;
            Some(json!({ "workspace": plugin::context::workspace_json(&session, panes) }))
        }),
        Closing::Window(id) => t.list_windows().ok().and_then(|windows| {
            let window = windows.into_iter().find(|w| w.window_id == id)?;
            Some(json!({ "tab": plugin::context::tab_json(&window, panes) }))
        }),
        Closing::Pane(id) => panes
            .iter()
            .find(|pane| pane.pane_id == id)
            .map(|pane| json!({ "pane": plugin::context::pane_json(pane) })),
    };
    let mut payload = record.unwrap_or_else(|| json!({ id_key: id }));
    payload["target"] = json!(target);
    payload
}

/// Runs each command hooked to `event` to the end, in order, and stops at the first that
/// refuses. What a hook prints on stdout is dropped, and a refusal carries the end of what it
/// wrote to stderr. Its time limit is the create hooks'. It is answered when its own shell
/// ends, plus a moment to read what the shell wrote ([`Streams::VETO`]): a process it left
/// running that holds its output is not waited for, so a yes is not turned into a refusal.
fn veto(ctx: &PluginCtx, kind: Hook, event: &str, built: &Built) -> anyhow::Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let dir = run::hook_dir(&built.env, &home);
    let env = environment(ctx, &built.env);
    let limit = create::hook_limit(ctx.cfg.sessions.hook_timeout_seconds);
    for command in ctx.cfg.hooks.commands(event) {
        let ran = create::run_command(
            command,
            &home,
            &env,
            built.json.as_bytes(),
            dir.as_deref(),
            limit,
            Streams::VETO,
        )
        .with_context(|| format!("running the {} {command}", kind.label()))?;
        create::require_success(kind, command, ran)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::tests_support::test_paths;
    use crate::session::create::{Hook, HookFailure};
    use crate::session::worktree::WorktreeRecord;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// A context whose config is `toml`, on a tmux nothing listens to: the payload then holds
    /// the ids a close names and nothing more, which is all these need.
    struct Rig {
        dir: tempfile::TempDir,
        cfg: Config,
        tmux: crate::tmux::Tmux,
        paths: crate::paths::Paths,
    }

    impl Rig {
        fn new(toml: &str) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("out");
            std::fs::create_dir_all(&out).unwrap();
            let cfg = toml::from_str(&toml.replace("$OUT", out.to_str().unwrap()))
                .expect("the test config parses");
            Rig {
                paths: test_paths(dir.path()),
                dir,
                cfg,
                tmux: crate::tmux::Tmux::with_socket_name("taw-hooks-veto-test-no-server"),
            }
        }

        fn ctx(&self) -> PluginCtx<'_> {
            PluginCtx {
                tmux: &self.tmux,
                cfg: &self.cfg,
                paths: &self.paths,
                // A veto runs where it is asked, never through the supervisor.
                taw_bin: Path::new("/nonexistent/taw"),
                server_env: None,
            }
        }

        fn out(&self, name: &str) -> PathBuf {
            self.dir.path().join("out").join(name)
        }

        /// Ends the process a hook left running, whose pid it wrote to `holder.pid`.
        fn kill_holder(&self) {
            let pid = std::fs::read_to_string(self.out("holder.pid")).unwrap_or_default();
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    fn refusal(result: anyhow::Result<()>) -> HookFailure {
        result
            .expect_err("the hook should have refused")
            .downcast::<HookFailure>()
            .expect("a refusal is a HookFailure, which is what makes it hook_refused")
    }

    #[test]
    fn a_close_nobody_has_a_hook_for_goes_ahead_without_running_anything() {
        let rig = Rig::new("[hooks]\n\"agent.done\" = \"touch $OUT/ran\"\n");

        assert!(pre_close(&rig.ctx(), Closing::Session("$3")).is_ok());
        assert!(!rig.out("ran").exists());
    }

    #[test]
    fn a_hook_that_exits_zero_lets_the_close_go_ahead() {
        let rig = Rig::new("[hooks]\npre_close = \"touch $OUT/asked\"\n");

        assert!(pre_close(&rig.ctx(), Closing::Pane("%31")).is_ok());
        assert!(rig.out("asked").exists());
    }

    #[test]
    fn a_hook_that_exits_non_zero_refuses_and_its_last_line_of_stderr_is_the_reason() {
        let rig = Rig::new(
            "[hooks]\npre_close = \"echo checking >&2; echo 'agent is working' >&2; exit 1\"\n",
        );

        let failure = refusal(pre_close(&rig.ctx(), Closing::Session("$3")));

        assert!(matches!(
            failure,
            HookFailure::Exited {
                hook: Hook::PreClose,
                ..
            }
        ));
        assert_eq!(failure.summary(), "pre-close hook: agent is working");
    }

    /// A refusal quotes what the hook said, and a hook can say a great deal (`curl -v`): what
    /// the message carries is the end of it, which is where the reason is.
    #[test]
    fn a_refusal_that_followed_a_great_deal_of_output_carries_only_the_end_of_it() {
        let rig = Rig::new(
            "[hooks]\npre_close = \"yes noise | head -c 300000 >&2; echo 'the reason' >&2; exit 1\"\n",
        );

        let failure = refusal(pre_close(&rig.ctx(), Closing::Session("$3")));

        assert_eq!(failure.summary(), "pre-close hook: the reason");
        let message = failure.to_string();
        assert!(message.len() < 20 * 1024, "{} bytes", message.len());
        assert!(message.ends_with("noise\nthe reason"), "{message}");
    }

    #[test]
    fn a_refusal_that_says_nothing_is_its_exit_status() {
        let rig = Rig::new("[hooks]\npre_close = \"exit 4\"\n");

        let failure = refusal(pre_close(&rig.ctx(), Closing::Window("@7")));

        assert_eq!(failure.summary(), "pre-close hook refused (exit status 4)");
    }

    #[test]
    fn every_command_of_a_list_is_asked_and_the_first_refusal_stops_the_rest() {
        let rig = Rig::new(
            "[hooks]\npre_close = [\"touch $OUT/first\", \"echo no >&2; exit 1\", \"touch $OUT/third\"]\n",
        );

        let failure = refusal(pre_close(&rig.ctx(), Closing::Session("$3")));

        assert_eq!(failure.summary(), "pre-close hook: no");
        assert!(rig.out("first").exists());
        assert!(!rig.out("third").exists(), "asked after a refusal");
    }

    #[test]
    fn a_hook_is_told_what_is_being_closed_by_the_environment_and_the_json() {
        let rig = Rig::new(
            "[hooks]\npre_close = \"env | grep '^TAW_' | sort > $OUT/env; cat > $OUT/json\"\n",
        );

        for (closing, target, id_var, id) in [
            (Closing::Session("$3"), "session", "TAW_SESSION_ID", "$3"),
            (Closing::Window("@7"), "window", "TAW_WINDOW", "@7"),
            (Closing::Pane("%31"), "pane", "TAW_PANE", "%31"),
        ] {
            pre_close(&rig.ctx(), closing).unwrap();

            let env = std::fs::read_to_string(rig.out("env")).unwrap();
            assert!(env.contains("TAW_EVENT=pre_close\n"), "{env}");
            assert!(env.contains(&format!("TAW_TARGET={target}\n")), "{env}");
            assert!(env.contains(&format!("{id_var}={id}\n")), "{env}");
            let json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(rig.out("json")).unwrap()).unwrap();
            assert_eq!(json["event"], "pre_close");
            assert_eq!(json["target"], target);
        }
    }

    /// The hook has `[sessions] hook_timeout_seconds` to answer in, as the create hooks do,
    /// and one that takes longer refuses too.
    #[test]
    fn a_hook_that_runs_past_the_sessions_hook_timeout_refuses() {
        let rig =
            Rig::new("[sessions]\nhook_timeout_seconds = 0.3\n[hooks]\npre_close = \"sleep 30\"\n");

        let started = Instant::now();
        let failure = refusal(pre_close(&rig.ctx(), Closing::Session("$3")));

        assert_eq!(failure.to_string(), "pre-close hook took longer than 0.3 s");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
    }

    /// `0` is no limit for these as it is for the create hooks (`hook_limit`): a hook that takes
    /// a while is not stopped at a limit of no seconds, and says what it has to say when it is
    /// done. The README and COMPAT say so.
    #[test]
    fn a_hook_timeout_of_zero_is_no_limit_for_a_hook_that_can_refuse() {
        let rig = Rig::new(
            "[sessions]\nhook_timeout_seconds = 0\n[hooks]\npre_close = \"sleep 1; echo 'the reason' >&2; exit 1\"\n",
        );

        let started = Instant::now();
        let failure = refusal(pre_close(&rig.ctx(), Closing::Session("$3")));

        assert_eq!(failure.summary(), "pre-close hook: the reason");
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
    }

    /// Something the hook left running keeps its stderr open, and the close is waiting on the
    /// answer: the hook said yes, and that is the answer, a moment after its shell has ended.
    /// Waited for, it would count against the limit and turn a yes into a refusal.
    #[test]
    fn a_hook_that_says_yes_and_leaves_a_process_holding_its_output_is_not_a_refusal() {
        let rig = Rig::new(
            "[sessions]\nhook_timeout_seconds = 4\n[hooks]\npre_close = \"sleep 30 & echo $! > $OUT/holder.pid\"\n",
        );

        let started = Instant::now();
        let asked = pre_close(&rig.ctx(), Closing::Session("$3"));
        let took = started.elapsed();

        rig.kill_holder();
        assert!(asked.is_ok(), "{asked:?}");
        assert!(took < Duration::from_secs(3), "waited {took:?}");
    }

    /// A refusal says why all the same: what the hook wrote before it ended is read.
    #[test]
    fn a_hook_that_says_no_and_leaves_a_process_behind_still_says_why_and_at_once() {
        let rig = Rig::new(
            "[sessions]\nhook_timeout_seconds = 4\n[hooks]\npre_close = \"echo 'the reason' >&2; sleep 30 & echo $! > $OUT/holder.pid; exit 1\"\n",
        );

        let started = Instant::now();
        let asked = pre_close(&rig.ctx(), Closing::Session("$3"));
        let took = started.elapsed();

        rig.kill_holder();
        assert_eq!(refusal(asked).summary(), "pre-close hook: the reason");
        assert!(took < Duration::from_secs(3), "waited {took:?}");
    }

    #[test]
    fn the_time_limit_of_an_event_hook_is_not_the_one_a_veto_answers_in() {
        // `[hooks] timeout_seconds` is for hooks that run in the background: a hook taw waits
        // for is bound by the create hooks' limit, which is 10 seconds by default.
        let rig = Rig::new("[hooks]\ntimeout_seconds = 0.1\npre_close = \"sleep 1; echo fine\"\n");

        assert!(pre_close(&rig.ctx(), Closing::Session("$3")).is_ok());
    }

    fn record(dir: &Path) -> WorktreeRecord {
        WorktreeRecord {
            workspace_id: "w9".to_string(),
            session_name: "repo--feat-x".to_string(),
            path: dir.to_path_buf(),
            branch: "feat/x".to_string(),
            base: None,
            repo_root: PathBuf::from("/home/me/repo"),
            source_workspace_id: "w1".to_string(),
            created_unix_ms: 0,
            machine: None,
            real_path: None,
            real_repo_root: None,
        }
    }

    #[test]
    fn a_worktree_removal_a_hook_refuses_names_the_hook_and_says_why() {
        let rig = Rig::new(
            "[hooks]\npre_worktree_remove = \"echo 'feat/x is not merged' >&2; exit 1\"\n",
        );

        let failure = refusal(pre_worktree_remove(&rig.ctx(), &record(rig.dir.path())));

        assert!(matches!(
            failure,
            HookFailure::Exited {
                hook: Hook::PreWorktreeRemove,
                ..
            }
        ));
        assert_eq!(
            failure.summary(),
            "pre-worktree-remove hook: feat/x is not merged"
        );
    }

    #[test]
    fn a_worktree_removal_is_told_which_worktree_and_where_it_is() {
        let rig = Rig::new(
            "[hooks]\npre_worktree_remove = \"env | grep '^TAW_' | sort > $OUT/env; pwd > $OUT/pwd; cat > $OUT/json\"\n",
        );
        let checkout = rig.dir.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();

        pre_worktree_remove(&rig.ctx(), &record(&checkout)).unwrap();

        let env = std::fs::read_to_string(rig.out("env")).unwrap();
        assert!(env.contains("TAW_EVENT=pre_worktree_remove\n"), "{env}");
        assert!(env.contains("TAW_SESSION=repo--feat-x\n"), "{env}");
        assert!(
            env.contains(&format!("TAW_DIR={}\n", checkout.display())),
            "{env}"
        );
        // The hook starts in the checkout it is asked about.
        let pwd = std::fs::read_to_string(rig.out("pwd")).unwrap();
        assert_eq!(
            std::fs::canonicalize(pwd.trim()).unwrap(),
            std::fs::canonicalize(&checkout).unwrap()
        );
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(rig.out("json")).unwrap()).unwrap();
        assert_eq!(json["worktree"]["branch"], "feat/x");
    }

    /// The checkout a worktree hook is asked about may be one it cannot enter: the hook is
    /// still asked, and starts where taw was.
    #[test]
    fn a_veto_whose_directory_cannot_be_entered_is_still_asked() {
        use std::os::unix::fs::PermissionsExt;
        let rig = Rig::new("[hooks]\npre_worktree_remove = \"touch $OUT/asked\"\n");
        let locked = rig.dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let asked = pre_worktree_remove(&rig.ctx(), &record(&locked));

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(asked.is_ok(), "{asked:?}");
        assert!(rig.out("asked").exists());
    }

    #[test]
    fn a_worktree_removal_with_no_hook_goes_ahead() {
        let rig = Rig::new("[hooks]\npre_close = \"exit 1\"\n");

        assert!(pre_worktree_remove(&rig.ctx(), &record(rig.dir.path())).is_ok());
    }
}
