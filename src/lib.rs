//! tmux-agent-wizard (`taw`): herdr-style agent awareness for tmux.

pub mod agent_resume;
pub(crate) mod atomic_file;
pub mod attention;
pub mod claude_flags;
pub mod cli;
pub mod config;
pub mod crash;
pub mod detect;
pub mod git;
pub mod hooks;
pub mod image;
pub mod integration;
pub mod machine;
pub mod notify;
pub mod paths;
pub mod plugin;
pub mod rail;
pub mod sandbox;
pub mod session;
pub mod store;
#[cfg(all(test, unix))]
pub(crate) mod test_exec;
#[cfg(all(test, unix))]
pub(crate) mod test_root;
pub mod tmux;
pub mod tui;
pub mod watch;
