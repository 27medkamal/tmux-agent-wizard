//! Machines taw lists besides this one: the `[machines]` table of the config, the ssh runner every
//! connection goes through, and `taw --machine`.
//!
//! Nothing here runs unless a machine is used. With no `[machines]` table taw behaves as it did
//! before this module existed: no file is written, no process is started, no tmux command changes.

pub mod actions;
pub mod bounded;
pub mod clean;
pub mod config;
pub mod conn;
pub mod edit;
pub mod feed;
pub mod files;
pub mod forward;
pub mod machines;
pub mod ssh;
pub mod terminfo;
pub mod wire;
pub mod words;

/// One machine of `[machines]`, as the config names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    /// What the headers, the toasts and `--machine` call the machine.
    pub name: String,
    /// What is typed after `ssh`: a host, `user@host` or an alias of the user's ssh config.
    pub ssh: String,
    /// Where taw is on that machine, when it is not where a login shell there finds it. `~/` at
    /// the start is that machine's home.
    pub taw: Option<String>,
}

/// What the list knows of one machine at a moment: the words under its header and its last state.
/// What the app's connection to it keeps, and what the popup reads back from its file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MachineView {
    pub name: String,
    /// Connected, and heard from within the last 15 seconds.
    pub live: bool,
    /// What the header says of it (`connected: taw 0.1.0`, `no taw there: ...`).
    pub words: String,
    /// The short form of `words` while they wait for the person to act (`no taw there: install it`), else empty:
    /// words that change (`connecting`, `not answering`) have none. The list's width counts a short form and never
    /// the words that change.
    pub short: String,
    /// The last state its feed sent, kept while the machine is away.
    pub state: Option<wire::State>,
    /// Unix milliseconds of the last line of any kind heard from it.
    pub last_seen: Option<u64>,
    /// The machine is this machine's own tmux server.
    pub this_server: bool,
    /// No app keeps the connections, so nothing is known of the machine but its name: its header says how to
    /// connect it ([`words::NO_KEEPER`]).
    pub no_keeper: bool,
}

/// Splits an id of the list into the machine it belongs to and the id tmux knows it by: `studio:%16`
/// is `(Some("studio"), "%16")`, and this machine's own `%16` is `(None, "%16")`. tmux's own ids
/// never hold a `:`, so the first one ends the machine's name. Whatever comes before it is the name,
/// even when it is empty: a caller looks the name up and refuses one it does not know.
pub fn split_id(id: &str) -> (Option<&str>, &str) {
    match id.split_once(':') {
        Some((machine, tmux_id)) => (Some(machine), tmux_id),
        None => (None, id),
    }
}

/// Whether `id` is what a going-in on a machine may name: an id tmux makes ([`valid_tmux_id`]), or `new`
/// ([`words::NEW_ID`]), a new session.
pub fn valid_go_id(id: &str) -> bool {
    valid_tmux_id(id) || id == words::NEW_ID
}

/// Whether `id` is an id tmux makes: `%` (a pane), `$` (a session) or `@` (a window), and digits.
/// Checked before an id is sent to another machine, where it becomes an argument of a command.
pub fn valid_tmux_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some('%' | '$' | '@'))
        && !chars.as_str().is_empty()
        && chars.all(|c| c.is_ascii_digit())
}

/// Whether `id` is a session id tmux makes: `$` and digits.
pub fn valid_session_id(id: &str) -> bool {
    id.starts_with('$') && valid_tmux_id(id)
}

/// What the list calls the machine taw runs on, in a place where it has to be told apart from the
/// others: `this Mac` on macOS and `this machine` anywhere else.
pub fn this_machine_word() -> &'static str {
    if cfg!(target_os = "macos") {
        "this Mac"
    } else {
        "this machine"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_with_a_machine_splits_at_the_first_colon() {
        assert_eq!(split_id("studio:%16"), (Some("studio"), "%16"));
        assert_eq!(split_id("pi:$3"), (Some("pi"), "$3"));
        assert_eq!(split_id("studio:@5"), (Some("studio"), "@5"));
        assert_eq!(split_id("a:b:c"), (Some("a"), "b:c"));
    }

    #[test]
    fn an_id_of_this_machine_has_no_machine() {
        assert_eq!(split_id("%16"), (None, "%16"));
        assert_eq!(split_id("$3"), (None, "$3"));
        assert_eq!(split_id(""), (None, ""));
    }

    #[test]
    fn an_empty_name_before_the_colon_is_still_a_name_for_the_caller_to_refuse() {
        assert_eq!(split_id(":%1"), (Some(""), "%1"));
    }

    #[test]
    fn only_a_percent_dollar_or_at_sign_and_digits_is_a_tmux_id() {
        for good in ["%0", "%16", "$3", "@5", "@1234567"] {
            assert!(valid_tmux_id(good), "{good}");
        }
        for bad in [
            "",
            "%",
            "$",
            "@",
            "16",
            "%a",
            "%1a",
            "% 1",
            "%-1",
            "studio:%16",
            "%16 ",
            "%1\n",
            "%\u{663}",
            "w1:p3",
        ] {
            assert!(!valid_tmux_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn this_machine_is_named_for_the_platform() {
        let word = this_machine_word();
        if cfg!(target_os = "macos") {
            assert_eq!(word, "this Mac");
        } else {
            assert_eq!(word, "this machine");
        }
    }
}
