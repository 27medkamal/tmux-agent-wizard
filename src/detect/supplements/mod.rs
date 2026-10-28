//! taw's own detection rules, appended to a manifest after loader precedence.
//!
//! Each `<id>.toml` here holds only `[[rules]]` (no
//! manifest header) and is compiled in with `include_str!` rather than read from disk,
//! since it ships with the binary rather than being installed by a user. Applied by
//! [`crate::detect::loader::ManifestStore`] through
//! [`crate::detect::engine::LoadedManifest::append_supplement`].

/// The supplement rules TOML for `id`, when taw ships one.
pub fn text(id: &str) -> Option<&'static str> {
    const SUPPLEMENTS: &[(&str, &str)] = &[
        ("claude", include_str!("claude.toml")),
        ("codex", include_str!("codex.toml")),
    ];
    SUPPLEMENTS
        .iter()
        .find(|(supplement_id, _)| *supplement_id == id)
        .map(|(_, text)| *text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_have_a_supplement_and_unknown_agents_do_not() {
        assert!(text("claude").is_some());
        assert!(text("codex").is_some());
        assert!(text("docker").is_none());
        // A bundled agent with nothing of taw's to add: the other tests that need one use it.
        assert!(text("gemini").is_none());
    }
}
