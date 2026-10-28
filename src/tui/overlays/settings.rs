//! The settings overlay's body: the effective configuration, as TOML text.
//!
//! `Config` is `Deserialize`-only (deriving `Serialize` on it belongs to the config
//! task, not this one), so the text is written by hand. It doubles as the place the
//! herdr keys taw accepts but cannot honour inside tmux are documented, next to the
//! values they would have affected.

use crate::config::{
    AgentPanelSort, Config, Depth, KeysPreset, Layout, StatusIndicators, ToastDelivery,
};
use crate::paths::Paths;
use crate::tui::theme;

/// The settings overlay's body: the effective configuration as TOML, the file it came
/// from, and what taw does with the keys it accepts but cannot honour inside tmux.
pub fn settings_text(
    cfg: &Config,
    paths: &Paths,
    theme: &theme::Resolved,
    taw_bin: &std::path::Path,
) -> String {
    let mut out = String::new();
    let sort = match cfg.ui.agent_panel_sort {
        AgentPanelSort::Attention => "attention",
        AgentPanelSort::Sessions => "sessions",
    };
    let indicators = match cfg.ui.status_indicators {
        StatusIndicators::Dots => "dots",
        StatusIndicators::Symbols => "symbols",
    };
    let depth = match cfg.ui.depth {
        Depth::Sessions => "sessions",
        Depth::Agents => "agents",
        Depth::All => "all",
    };
    let layout = match cfg.ui.layout {
        Layout::Tree => "tree",
        Layout::Flat => "flat",
    };
    let preset = match cfg.keys.preset {
        KeysPreset::Modal => "modal",
        KeysPreset::Search => "search",
    };
    let delivery = match cfg.ui.toast.delivery {
        ToastDelivery::Off => "off",
        ToastDelivery::Herdr => "herdr",
        ToastDelivery::Terminal => "terminal",
        ToastDelivery::System => "system",
    };
    out.push_str(&format!("# config file: {}\n", paths.config_file.display()));
    out.push_str(&format!("# binary:      {}\n", taw_bin.display()));
    if !paths.config_file.exists() {
        out.push_str("# (no config file yet, so every value below is a default)\n");
    }
    out.push('\n');
    out.push_str("[theme]\n");
    out.push_str(&format!("name = \"{}\"\n", theme.name));
    out.push_str(&format!("auto_switch = {}\n", cfg.theme.auto_switch));
    // What each side draws, set or taken from `name` (a key left out is not shown as unset).
    for (key, appearance) in [
        ("dark_name", theme::Appearance::Dark),
        ("light_name", theme::Appearance::Light),
    ] {
        out.push_str(&format!(
            "{key} = \"{}\"\n",
            theme::name_for(&cfg.theme, appearance)
        ));
    }
    if cfg.theme.auto_switch {
        out.push_str(AUTO_SWITCH_NOTE);
    }
    if !cfg.theme.custom.is_empty() {
        out.push_str("\n[theme.custom]\n");
        for (token, value) in &cfg.theme.custom {
            out.push_str(&format!("{token} = \"{value}\"\n"));
        }
    }
    out.push_str("\n[ui]\n");
    out.push_str(&format!("sidebar_width = {}\n", cfg.ui.sidebar_width));
    out.push_str(&format!("layout = \"{layout}\"\n"));
    out.push_str(&format!("depth = \"{depth}\"\n"));
    out.push_str(&format!("preview = {}\n", cfg.ui.preview));
    out.push_str(&format!("show_git = {}\n", cfg.ui.show_git));
    out.push_str(&format!("labels = {}\n", cfg.ui.labels));
    out.push_str(&format!("titles = {}\n", cfg.ui.titles));
    out.push_str(&format!(
        "projects_in_filter = {}\n",
        cfg.ui.projects_in_filter
    ));
    out.push_str(&format!("agent_panel_sort = \"{sort}\"\n"));
    out.push_str(&format!("status_indicators = \"{indicators}\"\n"));
    out.push_str(&format!("confirm_close = {}\n", cfg.ui.confirm_close));
    out.push_str(&format!(
        "show_agent_labels_on_pane_borders = {}\n",
        cfg.ui.show_agent_labels_on_pane_borders
    ));
    for (name, rows) in [
        ("agents", &cfg.ui.sidebar.agents),
        ("spaces", &cfg.ui.sidebar.spaces),
    ] {
        out.push_str(&format!("\n[ui.sidebar.{name}]\n"));
        out.push_str(&format!("row_gap = {}\n", rows.row_gap));
        out.push_str(&format!("rows = {}\n", toml_rows(&rows.rows)));
        for (kind, by_agent) in &rows.rows_by_agent {
            out.push_str(&format!(
                "[ui.sidebar.{name}.rows_by_agent]\n{kind} = {}\n",
                toml_rows(by_agent)
            ));
        }
    }
    out.push_str("\n[ui.toast]\n");
    out.push_str(&format!("delivery = \"{delivery}\"\n"));
    out.push_str(&format!("delay_seconds = {}\n", cfg.ui.toast.delay_seconds));
    out.push_str("\n[ui.sound]\n");
    out.push_str(&format!("enabled = {}\n", cfg.ui.sound.enabled));
    out.push_str("\n[keys]\n");
    out.push_str(&format!("preset = \"{preset}\"\n"));
    for (name, spec) in &cfg.keys.bindings {
        out.push_str(&format!("{name} = {:?}\n", spec.to_vec()));
    }
    out.push_str("\n[worktrees]\n");
    out.push_str(&format!(
        "directory = \"{}\"\n",
        cfg.worktrees.directory.display()
    ));
    out.push_str("\n# accepted, no effect inside tmux\n");
    for note in NO_EFFECT_NOTES {
        out.push_str(&format!("#   {note}\n"));
    }
    if !theme.warnings.is_empty() {
        out.push_str("\n# warnings\n");
        for warning in &theme.warnings {
            out.push_str(&format!("#   {warning}\n"));
        }
    }
    out
}

/// What the dump says under `[theme]` when `auto_switch` is on: the name above it is the theme drawn now.
const AUTO_SWITCH_NOTE: &str = "# auto_switch is on: the name above is the theme drawn now, chosen by this terminal's\n\
# light or dark mode (dark_name, light_name), or name when tmux cannot say (before 3.6)\n";

/// herdr keys taw parses but cannot act on, listed in the settings overlay so the
/// difference is visible where the values are.
const NO_EFFECT_NOTES: &[&str] = &[
    "[terminal]: tmux owns shells and pane creation",
    "[ui] show_agent_labels_on_pane_borders: taw never writes pane-border-format;",
    "  use #{@agent_kind}/#{@agent_state} in your own tmux.conf instead",
    "[update] channel / version_check: updates come from the tmux plugin",
    "[keys]: the popup's own bindings are honoured; the tmux-level keys that open",
    "  it come from the @agent-wizard* tmux options, not from this table",
    "[onboarding]: no first-run flow",
];

/// Renders `rows` back as the TOML array-of-arrays the config file uses.
fn toml_rows(rows: &[Vec<crate::config::RowToken>]) -> String {
    let inner: Vec<String> = rows
        .iter()
        .map(|row| {
            let tokens: Vec<String> = row
                .iter()
                .map(|t| match (&t.fg, t.bold, t.dim, t.rules.is_empty()) {
                    (None, None, None, true) => format!("\"{}\"", t.token),
                    (fg, bold, dim, _) => {
                        let mut parts = vec![format!("token = \"{}\"", t.token)];
                        if let Some(fg) = fg {
                            parts.push(format!("fg = \"{fg}\""));
                        }
                        if let Some(bold) = bold {
                            parts.push(format!("bold = {bold}"));
                        }
                        if let Some(dim) = dim {
                            parts.push(format!("dim = {dim}"));
                        }
                        if !t.rules.is_empty() {
                            let rules: Vec<String> =
                                t.rules.iter().map(ToString::to_string).collect();
                            parts.push(format!("rules = [{}]", rules.join(", ")));
                        }
                        format!("{{ {} }}", parts.join(", "))
                    }
                })
                .collect();
            format!("[{}]", tokens.join(", "))
        })
        .collect();
    format!("[{}]", inner.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_overlay_shows_the_effective_config_its_path_and_the_no_effect_keys() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let resolved = theme::resolve("catppuccin", &cfg.theme.custom);
        let text = settings_text(
            &cfg,
            &paths,
            &resolved,
            std::path::Path::new("/usr/bin/taw"),
        );
        assert!(text.contains(&paths.config_file.display().to_string()));
        assert!(text.contains("/usr/bin/taw"));
        assert!(text.contains("[ui]\nsidebar_width = 26"));
        // The popup's own settings, which the tree reads every frame.
        assert!(text.contains("layout = \"tree\""), "{text}");
        // The default: the agents, with the shells one `l` away.
        assert!(text.contains("depth = \"agents\""), "{text}");
        assert!(text.contains("preview = true"), "{text}");
        assert!(text.contains("show_git = true"), "{text}");
        // The two switches for the row labels and titles, listed with the other popup-facing keys.
        assert!(text.contains("labels = true"), "{text}");
        assert!(text.contains("titles = true"), "{text}");
        assert!(text.contains("[keys]\npreset = \"modal\""), "{text}");
        assert!(text.contains("agent_panel_sort = \"attention\""));
        assert!(text.contains("status_indicators = \"symbols\""));
        assert!(text.contains("rows = [[\"state_icon\", \"workspace\", \"tab\"], [\"agent\"]]"));
        assert!(text.contains("no effect inside tmux"));
        assert!(text.contains("auto_switch"));
        assert!(text.contains("[keys]"), "{text}");
        assert!(text.contains("[terminal]"), "{text}");
    }
    /// The dump no longer lists the theme keys among those with no effect, and says what `auto_switch` does when
    /// it is on, and nothing of the kind when it is off.
    #[test]
    fn the_settings_overlay_says_what_auto_switch_does() {
        let text = |toml: &str| {
            let cfg: Config = toml::from_str(toml).unwrap();
            settings_text(
                &cfg,
                &Paths::from_env(),
                &theme::resolve("catppuccin", &cfg.theme.custom),
                std::path::Path::new("taw"),
            )
        };
        let on = text("[theme]\nauto_switch = true\n");
        assert!(on.contains("auto_switch = true\n"), "{on}");
        assert!(on.contains(AUTO_SWITCH_NOTE), "{on}");
        assert!(on.contains("light or dark mode"), "{on}");
        assert!(!text("").contains("light or dark mode"));
        for dump in [&on, &text("")] {
            assert!(
                !dump.contains("[theme] auto_switch / dark_name / light_name"),
                "{dump}"
            );
        }
    }

    /// The filter's directories switch is a popup-facing key, so the options overlay says
    /// which way it is set, off by default.
    #[test]
    fn the_settings_overlay_says_whether_the_filter_lists_directories() {
        let text = |toml: &str| {
            let cfg: Config = toml::from_str(toml).unwrap();
            settings_text(
                &cfg,
                &Paths::from_env(),
                &theme::resolve("catppuccin", &cfg.theme.custom),
                std::path::Path::new("taw"),
            )
        };
        assert!(text("").contains("projects_in_filter = false\n"));
        assert!(text("[ui]\nprojects_in_filter = true\n").contains("projects_in_filter = true\n"));
    }

    #[test]
    fn the_settings_overlay_reports_an_unknown_theme_name_as_a_warning() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let resolved = theme::resolve("nonesuch", &cfg.theme.custom);
        let text = settings_text(&cfg, &paths, &resolved, std::path::Path::new("taw"));
        assert!(text.contains("name = \"catppuccin\""), "{text}");
        assert!(text.contains("# warnings"));
        assert!(text.contains("nonesuch"));
    }
    #[test]
    fn the_settings_overlay_lists_the_keys_a_config_rebound() {
        let cfg: Config =
            toml::from_str("[keys]\npreset = \"search\"\nnew_session = [\"ctrl-p\", \"N\"]\n")
                .unwrap();
        let text = settings_text(
            &cfg,
            &Paths::from_env(),
            &theme::resolve("catppuccin", &cfg.theme.custom),
            std::path::Path::new("taw"),
        );
        assert!(text.contains("preset = \"search\""), "{text}");
        assert!(text.contains("new_session = [\"ctrl-p\", \"N\"]"), "{text}");
    }

    #[test]
    fn styled_row_tokens_round_trip_into_the_settings_toml() {
        let cfg: Config = toml::from_str(
            "[ui.sidebar.agents]\nrows = [[{ token = \"workspace\", fg = \"#ff0000\", bold = true }]]\n[ui.sidebar.agents.rows_by_agent]\nclaude = [[\"agent\"]]\n",
        )
        .unwrap();
        let text = settings_text(
            &cfg,
            &Paths::from_env(),
            &theme::resolve("catppuccin", &cfg.theme.custom),
            std::path::Path::new("taw"),
        );
        assert!(
            text.contains("rows = [[{ token = \"workspace\", fg = \"#ff0000\", bold = true }]]"),
            "{text}"
        );
        assert!(text.contains("claude = [[\"agent\"]]"), "{text}");
    }

    /// A token's rules are part of the effective configuration the options overlay lists.
    #[test]
    fn token_rules_round_trip_into_the_settings_toml() {
        let cfg: Config = toml::from_str(
            r##"[ui.sidebar.agents]
rows = [[{ token = "$load", fg = "#fff", rules = [{ gt = 80, fg = "#f55", bold = true }, { equals = "Local", ignore_case = true, hide = true }] }], [{ token = "agent", rules = [{ contains = "cl" }] }]]
"##,
        )
        .unwrap();
        let text = settings_text(
            &cfg,
            &Paths::from_env(),
            &theme::resolve("catppuccin", &cfg.theme.custom),
            std::path::Path::new("taw"),
        );
        assert!(
            text.contains(
                r##"rows = [[{ token = "$load", fg = "#fff", rules = [{ gt = 80.0, fg = "#f55", bold = true }, { equals = "Local", ignore_case = true, hide = true }] }], [{ token = "agent", rules = [{ contains = "cl" }] }]]"##
            ),
            "{text}"
        );
        // The listing is itself a config the popup would read back the same.
        let line = text
            .lines()
            .find(|line| line.contains(r#"token = "$load""#))
            .expect("the agents' rows");
        let again: Config = toml::from_str(&format!("[ui.sidebar.agents]\n{line}\n")).unwrap();
        assert_eq!(again.ui.sidebar.agents.rows, cfg.ui.sidebar.agents.rows);
    }
}
