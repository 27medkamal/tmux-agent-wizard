//! What a hook inherits from tmux, read from a real server: its global environment, in every
//! shape a value can take, which server it is, and the environment of a session, which tmux puts
//! over the global one in a new pane.
//!
//! The values here are made up. Nothing prints an environment, since a real one holds secrets.

mod common;

use common::shell_env::ShellHome;
use taw::hooks::server_env::{ServerEnv, SessionEnv};
use taw::session::create::HookEnv;
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop. Never touches the
/// user's real tmux server. See `tests/tmux_client.rs` for why the name carries a counter and
/// why the server is started with `-f /dev/null`.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Tmux::with_socket_name(&format!("taw-test-hooks-env-{}-{n}", std::process::id()));
        let home = ShellHome::new();
        home.start_server(&t, &["-d", "-s", "one", "-x", "80", "-y", "24", "/bin/sh"]);
        Server(t, home)
    }

    fn set(&self, args: &[&str]) {
        let mut command = vec!["set-environment"];
        command.extend_from_slice(args);
        self.0.run_ok(&command).unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

fn value<'a>(env: &'a HookEnv, name: &str) -> Option<&'a str> {
    env.get(name)
}

/// What taw says about an event about the session `id`.
fn about(id: &str) -> Vec<(String, String)> {
    vec![("TAW_SESSION_ID".to_string(), id.to_string())]
}

#[test]
fn the_global_environment_comes_back_as_it_was_set_whatever_a_value_holds() {
    let server = Server::start();
    server.set(&["-g", "NTFY_TOPIC", "taw-hooks-test-topic"]);
    // A value that spans lines, and whose second line looks like a variable of its own.
    server.set(&["-g", "MULTI", "line one\nSPURIOUS=yes\n  --indent"]);
    server.set(&["-g", "SPECIAL", "has \"quotes\" $dollar `tick` back\\slash"]);
    server.set(&["-g", "EMPTY", ""]);
    server.set(&["-g", "UNI", "caf\u{e9}"]);

    let env = ServerEnv::read(&server.0).unwrap().for_hook(None, &[]);

    assert_eq!(value(&env, "NTFY_TOPIC"), Some("taw-hooks-test-topic"));
    assert_eq!(
        value(&env, "MULTI"),
        Some("line one\nSPURIOUS=yes\n  --indent")
    );
    assert_eq!(
        value(&env, "SPURIOUS"),
        None,
        "a line of a value is no variable"
    );
    assert_eq!(
        value(&env, "SPECIAL"),
        Some("has \"quotes\" $dollar `tick` back\\slash")
    );
    assert_eq!(value(&env, "EMPTY"), Some(""));
    assert_eq!(value(&env, "UNI"), Some("caf\u{e9}"));
}

/// A variable set with `set-environment -h` is left out of `show-environment`, and tmux passes it
/// to no pane. A hook is given it all the same, which is what the README's `set-environment -gh`
/// advice for a token needs. A variable that was removed (`set-environment -r`) is not there, and
/// is taken out of what the process that starts the hook has.
#[test]
fn a_hidden_variable_is_there_and_a_removed_one_is_not() {
    let server = Server::start();
    server.set(&["-gh", "HIDDEN_TOKEN", "taw-hooks-test-hidden"]);
    server.set(&["-g", "SOON_GONE", "x"]);
    server.set(&["-gr", "SOON_GONE"]);

    let env = ServerEnv::read(&server.0).unwrap().for_hook(None, &[]);

    assert_eq!(value(&env, "HIDDEN_TOKEN"), Some("taw-hooks-test-hidden"));
    assert_eq!(value(&env, "SOON_GONE"), None);
    assert_eq!(env.unset, ["SOON_GONE"]);
}

/// `TMUX` names this server the way tmux names it for a pane, and is not the one the process
/// that started the server had, which the server keeps in its global environment.
#[test]
fn tmux_names_the_server_and_not_the_one_that_launched_it() {
    let server = Server::start();
    let socket_path = server
        .0
        .run(&["display-message", "-p", "#{socket_path}"])
        .unwrap();
    let pid = server.0.run(&["display-message", "-p", "#{pid}"]).unwrap();
    server.set(&["-g", "TMUX", "/tmp/tmux-501/somewhere-else,1,0"]);
    server.set(&["-g", "TMUX_PANE", "%77"]);

    let env = ServerEnv::read(&server.0).unwrap();

    let hook = env.for_hook(None, &about("$0"));
    assert_eq!(
        value(&hook, "TMUX"),
        Some(format!("{socket_path},{pid},0").as_str())
    );
    assert_eq!(value(&hook, "TMUX_PANE"), None);
    assert_eq!(
        value(&env.for_hook(None, &[]), "TMUX"),
        Some(format!("{socket_path},{pid},-1").as_str())
    );
}

#[test]
fn a_server_that_is_not_running_cannot_be_read() {
    let nobody = Tmux::with_socket_name("taw-test-hooks-env-nobody-home");

    assert!(ServerEnv::read(&nobody).is_err());
}

/// A pane a server starts in a session now: what it prints of the variables named, one to a line,
/// `unset` for one it does not have. This is what tmux builds, and what a hook is meant to be
/// given.
fn new_pane_has(server: &Server, names: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    let script = names
        .iter()
        .map(|name| format!("printf '%s\\n' \"${{{name}-unset}}\""))
        .collect::<Vec<_>>()
        .join("; ");
    server
        .0
        .run_ok(&[
            "new-window",
            "-d",
            "-t",
            "one",
            &format!(
                "{{ {script}; }} > '{0}.tmp' && mv '{0}.tmp' '{0}'",
                out.display()
            ),
        ])
        .unwrap();
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(&out) {
            return text.lines().map(str::to_string).collect();
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the new pane never wrote what it has");
}

/// The environment of a session is what tmux puts over the global one in a new pane of it: a
/// variable it sets replaces the global one, and one it removes is not there. A hook is given
/// what such a pane has.
#[test]
fn a_hooks_environment_is_the_one_tmux_builds_for_a_new_pane_of_the_session() {
    let server = Server::start();
    server.set(&["-g", "SSH_AUTH_SOCK", "global-sock"]);
    server.set(&["-g", "ONLY_GLOBAL", "from-global"]);
    server.set(&["-g", "KEPT_GLOBAL", "global-kept"]);
    server.set(&["-t", "one", "SSH_AUTH_SOCK", "session-sock"]);
    server.set(&["-t", "one", "ONLY_SESSION", "from-session"]);
    server.set(&["-t", "one", "-r", "ONLY_GLOBAL"]);
    let names = [
        "SSH_AUTH_SOCK",
        "ONLY_GLOBAL",
        "KEPT_GLOBAL",
        "ONLY_SESSION",
    ];

    let own = SessionEnv::read(&server.0, "$0").expect("the session is there");
    let hook = ServerEnv::read(&server.0)
        .unwrap()
        .for_hook(Some(&own), &about("$0"));

    let ours: Vec<String> = names
        .iter()
        .map(|name| value(&hook, name).unwrap_or("unset").to_string())
        .collect();
    assert_eq!(
        ours,
        ["session-sock", "unset", "global-kept", "from-session"]
    );
    // (A session also holds a removal for each name in `update-environment` that the client it
    // was made from did not have, `DISPLAY` and the like, so this is not the only one.)
    assert!(hook.unset.iter().any(|name| name == "ONLY_GLOBAL"));
    assert!(hook.unset.iter().all(|name| value(&hook, name).is_none()));
    // And that is what a pane started in that session now has.
    assert_eq!(new_pane_has(&server, &names), ours);
}

/// A variable a session hides (`set-environment -t -h`) is read as the global one is: a hook has
/// it, and no pane does. It is the session's own, so it goes over a global one of the same name.
#[test]
fn a_variable_a_session_hides_reaches_a_hook_over_the_global_one_of_its_name() {
    let server = Server::start();
    server.set(&["-g", "PUSH_TOKEN", "global-token"]);
    server.set(&["-t", "one", "-h", "PUSH_TOKEN", "hidden-in-session"]);
    server.set(&["-t", "one", "-h", "ONLY_HIDDEN", "hidden-only-here"]);

    let own = SessionEnv::read(&server.0, "$0").expect("the session is there");
    let hook = ServerEnv::read(&server.0)
        .unwrap()
        .for_hook(Some(&own), &about("$0"));

    assert_eq!(value(&hook, "PUSH_TOKEN"), Some("hidden-in-session"));
    assert_eq!(value(&hook, "ONLY_HIDDEN"), Some("hidden-only-here"));
}

/// Each session has an environment of its own, so an event about one is not given another's.
#[test]
fn each_session_has_its_own_environment_and_one_that_is_not_there_has_none() {
    let server = Server::start();
    server
        .0
        .run_ok(&["new-session", "-d", "-s", "two", "/bin/sh"])
        .unwrap();
    server.set(&["-g", "SSH_AUTH_SOCK", "global-sock"]);
    server.set(&["-t", "one", "SSH_AUTH_SOCK", "sock-of-one"]);
    server.set(&["-t", "two", "SSH_AUTH_SOCK", "sock-of-two"]);
    let global = ServerEnv::read(&server.0).unwrap();
    let id_of = |name: &str| {
        server
            .0
            .run(&[
                "display-message",
                "-p",
                "-t",
                &format!("={name}:"),
                "#{session_id}",
            ])
            .unwrap()
    };

    for (name, sock) in [("one", "sock-of-one"), ("two", "sock-of-two")] {
        let id = id_of(name);
        let own = SessionEnv::read(&server.0, &id).expect("the session is there");
        let hook = global.for_hook(Some(&own), &about(&id));
        assert_eq!(value(&hook, "SSH_AUTH_SOCK"), Some(sock), "{name}");
    }
    assert!(
        SessionEnv::read(&server.0, "$999999").is_none(),
        "a session that is not there has no environment"
    );
    assert!(
        SessionEnv::read(
            &Tmux::with_socket_name("taw-test-hooks-env-nobody-home"),
            "$0"
        )
        .is_none()
    );
}
