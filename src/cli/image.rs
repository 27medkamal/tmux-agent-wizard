//! `taw image ...`: the image on this machine's clipboard into an agent's pane, here or on a machine of
//! `[machines]` (`crate::image`).

use clap::Subcommand;
use serde_json::{Value, json};

use super::{Ctx, Sub};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Paste the image on this machine's clipboard into a pane, here or on a machine of [machines]
    Paste {
        /// The pane, as tmux names it (%3); with --machine, that machine's own id
        #[arg(long, value_name = "PANE_ID")]
        pane: String,
        /// The machine of [machines] the pane is on
        #[arg(long, value_name = "NAME")]
        machine: Option<String>,
        /// The tmux client whose screen started this: a failure that comes after that screen has gone is shown on it
        #[arg(long, value_name = "CLIENT", hide = true)]
        client: Option<String>,
    },
    /// Take a PNG on standard input into a pane of this machine (what paste --machine runs there)
    Receive {
        #[arg(long, value_name = "PANE_ID")]
        pane: String,
        /// How many bytes the image has: nothing is written or pasted unless exactly that many arrive
        #[arg(long, value_name = "N")]
        bytes: usize,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Paste {
            pane,
            machine,
            client: _,
        } => (
            "paste",
            crate::image::paste(&ctx.tmux, &ctx.paths, &pane, machine.as_deref()).map(|pasted| {
                json!({ "pane": pasted.pane, "machine": pasted.machine, "path": pasted.path })
            }),
        ),
        Cmd::Receive { pane, bytes } => ("receive", receive(ctx, &pane, bytes)),
    }
}

fn receive(ctx: &Ctx, pane: &str, bytes: usize) -> anyhow::Result<Value> {
    let png = crate::image::read_stdin(bytes)?;
    let path = crate::image::deliver_here(&ctx.tmux, &ctx.paths, pane, &png)?;
    Ok(json!({ "pane": pane, "path": path.display().to_string() }))
}
