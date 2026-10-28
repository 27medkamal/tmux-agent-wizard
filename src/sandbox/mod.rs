//! Sandboxed agents: an agent run in a container, in the pane `taw sandbox` is typed in.
//!
//! A sandbox is a profile, and a profile is a plain script taw runs with a short contract: taw runs it in the project directory as `<profile> <the agent's
//! command> [<its arguments>...]`, with five variables added, and the profile ends by becoming the
//! container runtime, with the agent as the program the container runs. That process is what the
//! watcher recognises (`detect::container`), and what draws 📦 on the agent's row. taw ships one
//! profile, `default`, built in.

pub mod mark;
pub mod profile;
pub mod report;
pub mod start;
pub mod typed;

/// taw's own profile, `default`: the file `taw sandbox new` copies, and what taw writes to
/// `<state dir>/sandbox/default` whenever it runs it.
pub const DEFAULT_PROFILE: &str = include_str!("default-profile.sh");
