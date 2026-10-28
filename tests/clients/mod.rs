//! Real clients for the tests of the popup against a real tmux server.
//!
//! tmux will not attach a client without a tty and a test process has none to give, so the
//! tty comes from a second throwaway server whose one pane runs `tmux -L <server> attach`;
//! dropping a [`Client`] kills that server, which detaches the client. Every client of one
//! server needs a label of its own, which keeps their second servers apart.

// Each test file that includes this module uses a subset of it; unused-item warnings would
// otherwise fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use taw::tmux::{Socket, Tmux};

// The file that includes this one has `mod common;` too: the second server needs the home and the
// history file that `common::shell_env` makes.
use crate::common::shell_env::ShellHome;

/// A real client attached to one of a server's sessions.
pub struct Client {
    outer: Tmux,
    /// The home the shells of the second server have, which goes with it.
    _home: ShellHome,
    /// The name the server knows this client by, which `-c` takes.
    pub name: String,
}

impl Client {
    /// The second server, the one that gives the client its terminal.
    pub fn outer(&self) -> &Tmux {
        &self.outer
    }

    /// Attaches to `session` and waits (bounded) for `inner` to see the client.
    pub fn attach(inner: &Tmux, session: &str) -> Client {
        Client::attach_as(inner, session, "")
    }

    /// [`Client::attach`] for one of several clients of a server: `label` keeps their second
    /// servers apart.
    pub fn attach_as(inner: &Tmux, session: &str, label: &str) -> Client {
        let socket = match &inner.socket {
            Some(Socket::Name(name)) => name.clone(),
            other => panic!("the test server is always on a named socket, got {other:?}"),
        };
        let known: Vec<String> = inner
            .list_clients()
            .unwrap_or_default()
            .into_iter()
            .map(|client| client.client_name)
            .collect();
        let outer = Tmux::with_socket_name(&format!("{socket}-outer{label}"));
        let attach = format!("'{}' -L {socket} attach -t {session}", inner.bin.display());
        let home = ShellHome::new();
        home.start_server(
            &outer,
            &["-d", "-s", "host", "-x", "80", "-y", "24", &attach],
        );
        // Owned before the wait, so a failure below still kills the outer server.
        let mut client = Client {
            outer,
            _home: home,
            name: String::new(),
        };
        for _ in 0..100 {
            let new = inner
                .list_clients()
                .unwrap_or_default()
                .into_iter()
                .find(|attached| !known.contains(&attached.client_name));
            if let Some(new) = new {
                client.name = new.client_name;
                return client;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no client attached to {socket} within 5s");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.outer.run_ok(&["kill-server"]);
    }
}

/// The session each client is looking at, by client name.
pub fn sessions_of(t: &Tmux, clients: &[&Client]) -> Vec<String> {
    let attached = t.list_clients().unwrap();
    clients
        .iter()
        .map(|client| {
            attached
                .iter()
                .find(|attached| attached.client_name == client.name)
                .unwrap_or_else(|| panic!("no client {}", client.name))
                .session_name
                .clone()
        })
        .collect()
}
