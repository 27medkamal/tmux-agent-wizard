//! Runs the popup TUI on its own, before `taw ui` exists to launch it.
//!
//! The CLI task wires `taw ui` to `tui::run`; this example is how the popup is exercised
//! by hand in the meantime, and it stays afterwards as the smallest possible harness for
//! it: no clap, no subcommands, nothing between a tmux server and the TUI.
//!
//! It talks to whichever server `Tmux::from_env` resolves (`$TAW_TMUX_SOCKET_NAME`,
//! `$TAW_TMUX_SOCKET_PATH`, else the `$TMUX` this process runs inside), so a throwaway
//! test server is reached by running it there:
//!
//! ```sh
//! cargo build --example tui_demo
//! tmux -L taw-demo -f /dev/null new-session -d -s one
//! tmux -L taw-demo display-popup -E -w 90% -h 80% "$PWD/target/debug/examples/tui_demo"
//! ```
fn main() -> anyhow::Result<()> {
    let tmux = taw::tmux::Tmux::from_env();
    let paths = taw::paths::Paths::from_env();
    let config = taw::config::load(&paths)?;
    let bin = std::env::current_exe()?;
    // No origin pane: this harness is documented above as being run from
    // `display-popup -E`, which is exactly the case where tmux passes none on; `taw ui
    // open` is the thing that resolves one, and this example deliberately skips the CLI.
    // No bar either: nothing here ever swapped a status line for it. And it starts on the
    // tree, not on the new-session picker.
    taw::tui::run(&tmux, &config, &paths, &bin, None, None, false, false)
}
