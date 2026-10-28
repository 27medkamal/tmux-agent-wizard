//! What a hook inherits from tmux, beside what taw tells it about the event.
//!
//! The process that fires an event is one of several, and each has an environment of its own. A
//! pane's shell has what tmux built for that pane: its global environment (what `set-environment
//! -g` in `tmux.conf` sets) with the environment of its session over it, which is where tmux keeps
//! the `SSH_AUTH_SOCK` and `DISPLAY` of the terminal that attached last. A `run-shell` job and a
//! popup have the same and no pane, and a `pipe-pane` child, which is what the watcher is, has
//! only the environment the tmux server was launched with. A hook that pushes to a phone keeps
//! its credentials in one of those places, and one that reaches an ssh agent needs the socket of
//! the session it is about, so which process happened to fire it must not decide whether it works.
//!
//! taw builds a hook's environment the way tmux builds a new pane's in the session the event is
//! about: tmux's global environment, then that session's own over it (a variable it sets replaces
//! the global one, and one it removes with `set-environment -r` is taken out), then `TMUX` for the
//! server that fired the event, so that a `tmux` command in a hook reaches that server whatever
//! socket it runs on. The hook's own `TAW_*` variables are added last and always win. An event
//! about no session, or about one that is gone, has the global environment alone.

use std::sync::{Arc, Mutex, PoisonError};

use crate::plugin::env::PANE_CONTEXT_VARS;
use crate::session::create::HookEnv;
use crate::tmux::Tmux;

/// tmux's own variables, which are never taken from its environment: `TMUX` is set from the
/// server itself, and `TMUX_PANE` belongs to one pane (the global copy is whatever pane launched
/// the server, if any did).
const NOT_INHERITED: [&str; 2] = ["TMUX", "TMUX_PANE"];

/// A variable of a `show-environment -s` listing: `Some(value)` when it is set and `None` when it
/// is removed (`set-environment -r`), which is what a new pane must not have, whatever the process
/// that starts it has.
type Entry = (String, Option<String>);

/// What one tmux environment does to another: the variables it sets, and the ones it removes.
/// Not both for one name: what is set last is what is there.
///
/// The values may be secrets: nothing here is printed or logged.
#[derive(Clone, Default, PartialEq, Eq)]
struct Layer {
    set: Vec<(String, String)>,
    removed: Vec<String>,
}

impl Layer {
    /// The layer a `show-environment -s` listing makes.
    fn from_listing(listing: &str) -> Layer {
        let mut layer = Layer::default();
        layer.put(entries(listing));
        layer
    }

    /// Puts `entries` on this, in order: a variable that is set replaces one of the same name and
    /// is added when there is none, and one that is removed takes the name out.
    fn put(&mut self, entries: impl IntoIterator<Item = Entry>) {
        for (name, value) in entries {
            self.set.retain(|(own, _)| *own != name);
            self.removed.retain(|own| *own != name);
            match value {
                Some(value) => self.set.push((name, value)),
                None => self.removed.push(name),
            }
        }
    }

    /// This as entries, for putting on another layer.
    fn to_entries(&self) -> Vec<Entry> {
        let set = self
            .set
            .iter()
            .map(|(name, value)| (name.clone(), Some(value.clone())));
        let removed = self.removed.iter().map(|name| (name.clone(), None));
        set.chain(removed).collect()
    }

    fn names(&self) -> Vec<&String> {
        self.set
            .iter()
            .map(|(name, _)| name)
            .chain(&self.removed)
            .collect()
    }
}

/// tmux's global environment, and which server it belongs to.
///
/// The values may be secrets: nothing here is printed or logged, and [`std::fmt::Debug`]
/// shows the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ServerEnv {
    global: Layer,
    /// `(server pid, socket path)`: what `TMUX` says about the server.
    server: Option<(String, String)>,
}

impl std::fmt::Debug for ServerEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerEnv")
            .field("names", &self.global.names())
            .field("server", &self.server)
            .finish()
    }
}

/// The environment of one session as tmux keeps it (`show-environment -t`): its own variables,
/// which tmux puts over the global ones in a new pane of that session, and the ones it removes.
/// It is where tmux copies what `update-environment` names (`SSH_AUTH_SOCK`, `DISPLAY`) from the
/// terminal a client attaches from, so it changes whenever one does, and a long-lived reader
/// asks again for each event.
///
/// The values may be secrets: nothing here is printed or logged, and [`std::fmt::Debug`]
/// shows the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SessionEnv {
    layer: Layer,
}

impl std::fmt::Debug for SessionEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionEnv")
            .field("names", &self.layer.names())
            .finish()
    }
}

impl SessionEnv {
    /// Reads the environment of the session `session_id` (`$3`), the hidden variables
    /// (`set-environment -h`) too, as [`ServerEnv::read`] does the global one. `None` when tmux
    /// cannot say: the session is gone, or the server is.
    pub fn read(t: &Tmux, session_id: &str) -> Option<SessionEnv> {
        let listing = t
            .run(&["show-environment", "-s", "-t", session_id])
            .map_err(|err| {
                log::debug!("could not read the environment of {session_id} for a hook: {err:#}");
            })
            .ok()?;
        let mut layer = Layer::from_listing(&listing);
        if let Ok(hidden) = t.run(&["show-environment", "-sh", "-t", session_id]) {
            layer.put(entries(&hidden));
        }
        Some(SessionEnv { layer })
    }
}

/// The tmux id (`$3`) of the session an event is about, from the variables taw says about it:
/// `None` for an event about no session, and for one that names none by a tmux id.
pub fn session_of(taw: &[(String, String)]) -> Option<&str> {
    taw.iter()
        .find(|(name, _)| name == "TAW_SESSION_ID")
        .map(|(_, id)| id.as_str())
        .filter(|id| {
            id.strip_prefix('$').is_some_and(|number| {
                !number.is_empty()
                    && number.bytes().all(|b| b.is_ascii_digit())
                    && number.parse::<u32>().is_ok()
            })
        })
}

impl ServerEnv {
    /// Reads the global environment of the server `t` talks to, and its socket and pid.
    ///
    /// Two `tmux` runs: the environment in the shell format that `show-environment -s`
    /// prints, in which a value that spans lines or holds a quote is unambiguous, and the
    /// variables tmux hides from its listing (`set-environment -h`). tmux passes a hidden
    /// variable to no pane, and a hook is given it all the same: the README offers
    /// `set-environment -gh` as the way to keep a token out of `tmux show-environment`, and a
    /// hook is where the token is wanted. A tmux too old to know that flag has no such
    /// variables.
    pub fn read(t: &Tmux) -> anyhow::Result<ServerEnv> {
        let listing = t.run(&[
            "display-message",
            "-p",
            "#{pid} #{socket_path}",
            ";",
            "show-environment",
            "-gs",
        ])?;
        let mut env = ServerEnv::from_listing(&listing);
        if let Ok(hidden) = t.run(&["show-environment", "-gsh"]) {
            env.global.put(entries(&hidden));
        }
        Ok(env)
    }

    /// The environment from what [`ServerEnv::read`] gets from tmux: a first line with the
    /// server's pid and socket path, then the variables.
    fn from_listing(listing: &str) -> ServerEnv {
        let (first, rest) = listing.split_once('\n').unwrap_or((listing, ""));
        let server = first
            .split_once(' ')
            .filter(|(pid, path)| {
                !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && !path.is_empty()
            })
            .map(|(pid, path)| (pid.to_string(), path.to_string()));
        ServerEnv {
            global: Layer::from_listing(rest),
            server,
        }
    }

    /// What to start a hook with, on top of the environment its process has: the environment
    /// tmux builds for a new pane of `session`, the one the event is about (tmux's global
    /// environment, the session's own over it, and what the session removes taken out), then
    /// `TMUX` for the server, then `taw`, the variables taw says about the event, which replace
    /// any variable of the same name. With no session, the global environment is all there is.
    ///
    /// `TMUX` names the server the way tmux does for a pane (`socket path,server pid,session
    /// number`), the session being the one the event is about (`TAW_SESSION_ID`), or `-1` for
    /// none, as tmux says.
    pub fn for_hook(&self, session: Option<&SessionEnv>, taw: &[(String, String)]) -> HookEnv {
        let mut layer = self.global.clone();
        if let Some(session) = session {
            layer.put(session.layer.to_entries());
        }
        let not_ours = |name: &str| {
            !NOT_INHERITED.contains(&name)
                && !PANE_CONTEXT_VARS.contains(&name)
                && !taw.iter().any(|(own, _)| own == name)
        };
        let mut env = HookEnv {
            set: layer
                .set
                .into_iter()
                .filter(|(name, _)| not_ours(name))
                .collect(),
            unset: layer
                .removed
                .into_iter()
                .filter(|name| not_ours(name))
                .collect(),
        };
        if let Some((pid, path)) = &self.server {
            let number = session_of(taw)
                .and_then(|id| id.strip_prefix('$'))
                .and_then(|number| number.parse::<u32>().ok())
                .map_or_else(|| "-1".to_string(), |number| number.to_string());
            env.set
                .push(("TMUX".to_string(), format!("{path},{pid},{number}")));
        }
        env.set.extend(taw.iter().cloned());
        env
    }
}

/// What follows the first line of `text`.
fn after_line(text: &str) -> &str {
    text.split_once('\n').map_or("", |(_, tail)| tail)
}

/// The entries of a `show-environment -s` listing, in the order listed: `NAME="value"; export
/// NAME;` for a variable that is set, with `\` before a `"`, `$`, `` ` `` or `\` in the value,
/// and `unset NAME;` for one that was removed. A value may span lines. A line that is neither
/// is skipped.
fn entries(listing: &str) -> Vec<Entry> {
    let mut found = Vec::new();
    let mut rest = listing;
    while !rest.is_empty() {
        if let Some(statement) = rest.strip_prefix("unset ") {
            let name = statement.lines().next().and_then(|l| l.strip_suffix(';'));
            if let Some(name) =
                name.filter(|name| !name.is_empty() && !name.contains([' ', '\t', '=', '\0']))
            {
                found.push((name.to_string(), None));
            }
            rest = after_line(rest);
            continue;
        }
        if rest.starts_with('\n') {
            rest = after_line(rest);
            continue;
        }
        let Some((name, value)) = rest.split_once("=\"") else {
            break;
        };
        if name.is_empty() || name.contains(['\n', ' ', '\t', '=', '\0']) {
            rest = after_line(rest);
            continue;
        }
        let mut text = String::new();
        let mut chars = value.char_indices();
        let mut end = None;
        while let Some((at, c)) = chars.next() {
            match c {
                '\\' => text.extend(chars.next().map(|(_, escaped)| escaped)),
                '"' => {
                    end = Some(at + c.len_utf8());
                    break;
                }
                c => text.push(c),
            }
        }
        let Some(end) = end else {
            // A value that never ends is a listing that was cut: nothing after it is known.
            break;
        };
        found.push((name.to_string(), Some(text)));
        rest = after_line(&value[end..]);
    }
    found
}

/// [`ServerEnv`] as a long-lived process keeps it: read the first time a hook needs it, and
/// held until told to forget it, so that a watcher pays for tmux's answer once and not for
/// every event.
#[derive(Debug, Default)]
pub struct ServerEnvCache {
    held: Mutex<Option<Arc<ServerEnv>>>,
}

impl ServerEnvCache {
    /// The environment of the server `t` talks to. A read that fails is not held, so the next
    /// call tries again.
    pub fn get(&self, t: &Tmux) -> Option<Arc<ServerEnv>> {
        self.get_with(|| ServerEnv::read(t))
    }

    fn get_with(&self, read: impl FnOnce() -> anyhow::Result<ServerEnv>) -> Option<Arc<ServerEnv>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_none() {
            match read() {
                Ok(env) => *held = Some(Arc::new(env)),
                Err(err) => {
                    log::debug!("could not read tmux's global environment for a hook: {err:#}");
                    return None;
                }
            }
        }
        held.clone()
    }

    /// Drops what was read, so that the next call reads tmux again: the watcher does this
    /// when its configuration is reloaded.
    pub fn forget(&self) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// Entries as they are listed: `Some` for a variable that is set, `None` for one removed.
    fn listed(vars: &[(&str, Option<&str>)]) -> Vec<Entry> {
        vars.iter()
            .map(|(name, value)| (name.to_string(), value.map(str::to_string)))
            .collect()
    }

    /// The global environment of a server known by nothing but its variables.
    fn global(set: &[(&str, &str)], removed: &[&str]) -> ServerEnv {
        ServerEnv {
            global: Layer {
                set: pairs(set),
                removed: removed.iter().map(|name| name.to_string()).collect(),
            },
            server: None,
        }
    }

    /// The environment of a session that sets `set` and removes `removed`.
    fn session(set: &[(&str, &str)], removed: &[&str]) -> SessionEnv {
        SessionEnv {
            layer: Layer {
                set: pairs(set),
                removed: removed.iter().map(|name| name.to_string()).collect(),
            },
        }
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    #[test]
    fn a_listing_of_plain_variables_is_read_in_order() {
        let listing = "NTFY_TOPIC=\"my-topic\"; export NTFY_TOPIC;\nEMPTY=\"\"; export EMPTY;\n\
                       PATH=\"/usr/bin:/bin\"; export PATH;\n";

        assert_eq!(
            entries(listing),
            listed(&[
                ("NTFY_TOPIC", Some("my-topic")),
                ("EMPTY", Some("")),
                ("PATH", Some("/usr/bin:/bin"))
            ])
        );
    }

    #[test]
    fn a_value_that_spans_lines_or_holds_quotes_comes_back_as_it_was_set() {
        // What `set-environment -g` was given: a multi-line value whose second line looks
        // like a variable of its own, and every character the shell format escapes.
        let listing = "MULTI=\"line one\nSPURIOUS=yes\n  --indent\"; export MULTI;\n\
                       SPECIAL=\"has \\\"quotes\\\" \\$dollar \\`tick\\` back\\\\slash\"; export SPECIAL;\n\
                       AFTER=\"1\"; export AFTER;\n";

        assert_eq!(
            entries(listing),
            listed(&[
                ("MULTI", Some("line one\nSPURIOUS=yes\n  --indent")),
                ("SPECIAL", Some("has \"quotes\" $dollar `tick` back\\slash")),
                ("AFTER", Some("1")),
            ])
        );
    }

    #[test]
    fn a_removed_variable_is_read_as_a_removal_and_stray_lines_are_skipped() {
        let listing = "unset GONE;\nKEPT=\"yes\"; export KEPT;\nnot a statement\n\n\
                       ALSO=\"kept\"; export ALSO;\nunset ALSO_GONE;\nunset ;\n";

        assert_eq!(
            entries(listing),
            listed(&[
                ("GONE", None),
                ("KEPT", Some("yes")),
                ("ALSO", Some("kept")),
                ("ALSO_GONE", None),
            ])
        );
    }

    #[test]
    fn a_variable_named_unset_is_a_variable_and_a_removal_of_it_is_a_removal() {
        assert_eq!(
            entries("unset=\"1\"; export unset;\nunset unset;\n"),
            listed(&[("unset", Some("1")), ("unset", None)])
        );
    }

    #[test]
    fn non_ascii_text_and_tabs_survive() {
        let listing = "UNI=\"caf\u{e9} \u{1f600}\"; export UNI;\nTAB=\"a\tb\"; export TAB;\n";

        assert_eq!(
            entries(listing),
            listed(&[("UNI", Some("caf\u{e9} \u{1f600}")), ("TAB", Some("a\tb"))])
        );
    }

    #[test]
    fn a_listing_that_was_cut_gives_what_was_whole() {
        let listing = "FIRST=\"1\"; export FIRST;\nCUT=\"never ends";

        assert_eq!(entries(listing), listed(&[("FIRST", Some("1"))]));
        assert!(entries("").is_empty());
    }

    #[test]
    fn the_first_line_names_the_server_and_the_rest_is_its_environment() {
        let env = ServerEnv::from_listing(
            "4711 /private/tmp/tmux-501/work\nA=\"1\"; export A;\nB=\"2\"; export B;\nunset C;\n",
        );

        assert_eq!(
            env.server,
            Some(("4711".to_string(), "/private/tmp/tmux-501/work".to_string()))
        );
        assert_eq!(env.global.set, pairs(&[("A", "1"), ("B", "2")]));
        assert_eq!(env.global.removed, vec!["C".to_string()]);
    }

    #[test]
    fn a_socket_path_with_spaces_is_one_path() {
        let env = ServerEnv::from_listing("77 /tmp/my dir/sock\n");

        assert_eq!(
            env.server,
            Some(("77".to_string(), "/tmp/my dir/sock".to_string()))
        );
    }

    #[test]
    fn an_answer_that_is_not_a_pid_and_a_path_names_no_server() {
        for first in ["", "no server", "abc /tmp/sock", "12 "] {
            let env = ServerEnv::from_listing(&format!("{first}\nA=\"1\"; export A;\n"));

            assert_eq!(env.server, None, "{first:?}");
        }
    }

    #[test]
    fn a_hook_gets_tmuxs_global_environment_and_the_variables_taw_says_win() {
        let env = global(
            &[
                ("NTFY_TOPIC", "my-topic"),
                ("TAW_EVENT", "from-the-global-environment"),
                ("PATH", "/usr/bin"),
            ],
            &[],
        );

        let hook = env.for_hook(
            None,
            &pairs(&[("TAW_EVENT", "agent.done"), ("TAW_PANE", "%9")]),
        );

        assert_eq!(hook.get("NTFY_TOPIC"), Some("my-topic"));
        assert_eq!(hook.get("PATH"), Some("/usr/bin"));
        assert_eq!(hook.get("TAW_EVENT"), Some("agent.done"));
        assert_eq!(hook.get("TAW_PANE"), Some("%9"));
        assert_eq!(
            hook.set
                .iter()
                .filter(|(name, _)| name == "TAW_EVENT")
                .count(),
            1,
            "one TAW_EVENT, or which one wins depends on the order"
        );
    }

    /// The way tmux builds a new pane's environment in a session: the global one, and the
    /// session's own over it. A variable the session sets replaces the global one, and this is
    /// where `SSH_AUTH_SOCK` and `DISPLAY` of the terminal that attached last are kept. One the
    /// session removes is taken out, and stays out of what the process that fired the event has.
    #[test]
    fn a_sessions_own_variables_go_over_the_global_ones_and_what_it_removes_is_taken_out() {
        let env = global(
            &[
                ("SSH_AUTH_SOCK", "global-sock"),
                ("ONLY_GLOBAL", "from-global"),
                ("DISPLAY", "global-display"),
            ],
            &[],
        );
        let own = session(
            &[("SSH_AUTH_SOCK", "session-sock"), ("ONLY_SESSION", "mine")],
            &["DISPLAY"],
        );

        let hook = env.for_hook(Some(&own), &[]);

        assert_eq!(hook.get("SSH_AUTH_SOCK"), Some("session-sock"));
        assert_eq!(hook.get("ONLY_SESSION"), Some("mine"));
        assert_eq!(hook.get("ONLY_GLOBAL"), Some("from-global"));
        assert_eq!(hook.get("DISPLAY"), None, "removed by the session");
        assert_eq!(hook.unset, vec!["DISPLAY".to_string()]);
        assert_eq!(
            hook.set
                .iter()
                .filter(|(name, _)| name == "SSH_AUTH_SOCK")
                .count(),
            1
        );
    }

    /// What is put last is what is there: a session that sets again what the global environment
    /// removes has it, and one the global environment removes is taken out of a hook's whether
    /// or not the session says anything of it.
    #[test]
    fn a_session_that_sets_what_the_global_environment_removes_has_it_and_a_global_removal_stands()
    {
        let env = global(&[], &["GONE_EVERYWHERE", "GONE_HERE_ONLY"]);
        let own = session(&[("GONE_HERE_ONLY", "back")], &[]);

        let hook = env.for_hook(Some(&own), &[]);

        assert_eq!(hook.get("GONE_HERE_ONLY"), Some("back"));
        assert_eq!(hook.unset, vec!["GONE_EVERYWHERE".to_string()]);
        // With no session, both stay out.
        assert_eq!(
            sorted(env.for_hook(None, &[]).unset),
            vec!["GONE_EVERYWHERE".to_string(), "GONE_HERE_ONLY".to_string()]
        );
    }

    /// With `@agent-wizard-herdr-env` on, tmux's global environment holds `HERDR_PANE_ID=%calling`,
    /// which is no pane of the event's: a hook is told its pane by `TAW_PANE`, so none of the pane,
    /// tab and workspace variables comes from the global or the session's environment.
    #[test]
    fn a_hook_inherits_no_pane_ids_from_tmux_environments() {
        let env = global(
            &[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "%calling"),
                ("HERDR_TAB_ID", "w1:t1"),
                ("TAW_WORKSPACE_ID", "w1"),
                ("NTFY_TOPIC", "my-topic"),
            ],
            &[],
        );
        let own = session(
            &[("HERDR_WORKSPACE_ID", "w9"), ("TAW_PANE_ID", "w9:p9")],
            &[],
        );

        let hook = env.for_hook(Some(&own), &pairs(&[("TAW_PANE", "%9")]));

        for name in PANE_CONTEXT_VARS {
            assert_eq!(hook.get(name), None, "{name}");
        }
        assert_eq!(hook.get("NTFY_TOPIC"), Some("my-topic"));
        assert_eq!(hook.get("HERDR_ENV"), Some("1"));
        assert_eq!(hook.get("TAW_PANE"), Some("%9"));
    }

    /// An event about no session, or one that is gone, has tmux's global environment alone.
    #[test]
    fn with_no_session_a_hook_has_the_global_environment_alone() {
        let env = global(&[("SSH_AUTH_SOCK", "global-sock")], &[]);

        let hook = env.for_hook(None, &[]);

        assert_eq!(hook.get("SSH_AUTH_SOCK"), Some("global-sock"));
        assert!(hook.unset.is_empty());
    }

    /// `TMUX` is what tmux sets for the server, and `TAW_*` is what taw says about the event: a
    /// session that sets a variable of either name, or `TMUX_PANE`, changes neither.
    #[test]
    fn a_session_cannot_change_what_taw_and_tmux_say_about_the_event() {
        let env = ServerEnv {
            global: Layer::default(),
            server: Some(("4711".to_string(), "/tmp/tmux-501/work".to_string())),
        };
        let own = session(
            &[
                ("TAW_EVENT", "from-the-session"),
                ("TMUX", "/tmp/tmux-501/other,1,0"),
                ("TMUX_PANE", "%77"),
                ("KEPT", "yes"),
            ],
            &["TAW_PANE"],
        );

        let hook = env.for_hook(
            Some(&own),
            &pairs(&[
                ("TAW_EVENT", "agent.done"),
                ("TAW_PANE", "%9"),
                ("TAW_SESSION_ID", "$3"),
            ]),
        );

        assert_eq!(hook.get("TAW_EVENT"), Some("agent.done"));
        assert_eq!(hook.get("TAW_PANE"), Some("%9"));
        assert_eq!(hook.get("TMUX"), Some("/tmp/tmux-501/work,4711,3"));
        assert_eq!(hook.get("TMUX_PANE"), None);
        assert_eq!(hook.get("KEPT"), Some("yes"));
        assert!(hook.unset.is_empty(), "{hook:?}");
        for name in ["TAW_EVENT", "TAW_PANE", "TMUX"] {
            assert_eq!(hook.set.iter().filter(|(own, _)| own == name).count(), 1);
        }
    }

    #[test]
    fn a_hook_is_told_which_server_and_which_session_the_way_tmux_tells_a_pane() {
        let env = ServerEnv {
            global: Layer::default(),
            server: Some(("4711".to_string(), "/tmp/tmux-501/work".to_string())),
        };

        let hook = env.for_hook(None, &pairs(&[("TAW_SESSION_ID", "$3")]));
        assert_eq!(hook.get("TMUX"), Some("/tmp/tmux-501/work,4711,3"));

        // A session that is gone, or an event about none, is `-1`, which is what tmux says.
        for id in ["", "w3", "$x"] {
            let hook = env.for_hook(None, &pairs(&[("TAW_SESSION_ID", id)]));
            assert_eq!(
                hook.get("TMUX"),
                Some("/tmp/tmux-501/work,4711,-1"),
                "{id:?}"
            );
        }
        assert_eq!(
            env.for_hook(None, &[]).get("TMUX"),
            Some("/tmp/tmux-501/work,4711,-1")
        );
    }

    /// The session an event is about is the one `TAW_SESSION_ID` names by its tmux id, and no
    /// other spelling is asked about.
    #[test]
    fn the_session_an_event_is_about_is_named_by_its_tmux_id_and_by_nothing_else() {
        assert_eq!(session_of(&pairs(&[("TAW_SESSION_ID", "$3")])), Some("$3"));
        assert_eq!(
            session_of(&pairs(&[("TAW_PANE", "%9"), ("TAW_SESSION_ID", "$12")])),
            Some("$12")
        );
        for id in [
            "",
            "w3",
            "$",
            "$x",
            "$-1",
            "$+3",
            "$3 ",
            "$99999999999",
            "3",
        ] {
            assert_eq!(
                session_of(&pairs(&[("TAW_SESSION_ID", id)])),
                None,
                "{id:?}"
            );
        }
        assert_eq!(session_of(&[]), None);
    }

    #[test]
    fn the_tmux_and_tmux_pane_a_global_environment_holds_are_not_passed_on() {
        // A server started from inside a pane of another server holds that pane's own.
        let env = ServerEnv {
            global: Layer {
                set: pairs(&[
                    ("TMUX", "/tmp/tmux-501/other,1,0"),
                    ("TMUX_PANE", "%77"),
                    ("KEPT", "yes"),
                ]),
                removed: Vec::new(),
            },
            server: Some(("4711".to_string(), "/tmp/tmux-501/work".to_string())),
        };

        let hook = env.for_hook(None, &[]);

        assert_eq!(hook.get("TMUX"), Some("/tmp/tmux-501/work,4711,-1"));
        assert_eq!(hook.get("TMUX_PANE"), None);
        assert_eq!(hook.get("KEPT"), Some("yes"));
        assert_eq!(
            hook.set.iter().filter(|(name, _)| name == "TMUX").count(),
            1
        );
    }

    #[test]
    fn with_no_server_known_a_hook_keeps_the_tmux_its_process_has() {
        let env = global(&[("TMUX", "/tmp/tmux-501/other,1,0")], &[]);

        assert_eq!(env.for_hook(None, &[]).get("TMUX"), None);
    }

    #[test]
    fn debug_shows_names_and_never_values() {
        let env = global(&[("PUSH_TOKEN", "hunter2")], &["OLD_TOKEN"]);
        let own = session(&[("SESSION_TOKEN", "swordfish")], &[]);
        let hook = env.for_hook(Some(&own), &[]);

        for shown in [format!("{env:?}"), format!("{own:?}"), format!("{hook:?}")] {
            assert!(
                !shown.contains("hunter2") && !shown.contains("swordfish"),
                "{shown}"
            );
        }
        assert!(format!("{env:?}").contains("PUSH_TOKEN"));
        assert!(format!("{own:?}").contains("SESSION_TOKEN"));
        assert!(format!("{hook:?}").contains("PUSH_TOKEN"));
        assert!(format!("{hook:?}").contains("OLD_TOKEN"));
    }

    #[test]
    fn the_cache_reads_once_until_told_to_forget_and_does_not_hold_a_failure() {
        let cache = ServerEnvCache::default();
        let reads = std::cell::Cell::new(0);
        let read = |fail: bool| {
            reads.set(reads.get() + 1);
            if fail {
                anyhow::bail!("no server running");
            }
            Ok(global(&[("N", &reads.get().to_string())], &[]))
        };

        assert!(cache.get_with(|| read(true)).is_none());
        assert!(
            cache.get_with(|| read(true)).is_none(),
            "a failure is retried"
        );
        let first = cache.get_with(|| read(false)).unwrap();
        assert_eq!(first.for_hook(None, &[]).get("N"), Some("3"));
        assert_eq!(reads.get(), 3);

        cache.get_with(|| read(false)).unwrap();
        cache.get_with(|| read(false)).unwrap();
        assert_eq!(reads.get(), 3, "held, not read again");

        cache.forget();
        let again = cache.get_with(|| read(false)).unwrap();
        assert_eq!(again.for_hook(None, &[]).get("N"), Some("4"));
    }
}
