//! The popup's overlays: the project picker, help, options, text prompts, a close
//! confirmation, and the lettered menus (rename, and the right-click context list).
//!
//! Each overlay is a small state machine over key events that returns a
//! [`Response`](state::Response); none of them touch tmux. Validation that needs to see
//! the world (does this path exist, is this agent name taken) is the app's job, so a
//! prompt reports a [`Response::Submit`](state::Response::Submit) and waits to be told
//! whether it was accepted.
//!
//! Split by concern, since one file held all three: [`state`] is what the overlays *are*
//! and how they answer a key, [`render`] is how they are drawn, and [`settings`] writes
//! the effective-configuration text the settings overlay shows. Everything public is
//! re-exported here, so callers use `overlays::Overlay` without caring which file it is
//! declared in.

pub mod render;
pub mod settings;
pub mod state;

pub use render::{AppLook, Chrome, centered, render};
pub use settings::settings_text;
pub use state::{
    Answers, Confirm, Help, Item, Menu, Overlay, Prompt, PromptKind, Response, RowAction, Scroll,
    also, help_keys, help_text, help_text_for,
};

/// What the overlays' tests share.
#[cfg(test)]
mod test_support {
    use crate::config::{KeySpec, Keys, KeysPreset};
    use crate::tui::keys::{self, Keymap};

    /// `preset`'s keymap with `[keys]` entries on top: each action and its keys, where an
    /// empty list leaves the action with no key at all.
    pub fn keymap_with(preset: KeysPreset, entries: &[(&str, &[&str])]) -> Keymap {
        let (keymap, diagnostics) = keys::from_config(&Keys {
            preset,
            bindings: entries
                .iter()
                .map(|(action, keys)| {
                    let keys = keys.iter().map(|key| (*key).to_string()).collect();
                    ((*action).to_string(), KeySpec::Many(keys))
                })
                .collect(),
        });
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        keymap
    }
}
