//! The popup's colour palette: eleven built-in themes plus `[theme.custom]` token
//! overrides, resolved once at startup.
//!
//! A theme is a flat table of fourteen tokens, the same names herdr's `[theme.custom]`
//! accepts, so a herdr config's overrides apply here unchanged. With `[theme]
//! auto_switch` on, the popup draws `dark_name` or `light_name` as its tmux client's
//! `#{client_theme}` says (tmux 3.6 and later), and `name` when the client's theme is
//! unknown or tmux is older ([`chosen_name`]).

use std::collections::BTreeMap;
use std::str::FromStr;

use ratatui::style::Color;

use crate::store::AgentState;

/// The default theme, used when `[theme] name` is unset or unknown.
pub const DEFAULT_NAME: &str = "catppuccin";

/// Every built-in theme name, in the order the settings overlay lists them.
pub const NAMES: &[&str] = &[
    "catppuccin",
    "catppuccin-latte",
    "terminal",
    "tokyo-night",
    "dracula",
    "nord",
    "gruvbox",
    "one-dark",
    "solarized",
    "kanagawa",
    "rose-pine",
    "vesper",
];

/// The fourteen colour tokens every theme defines. Overridable one by one from
/// `[theme.custom]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub sidebar_bg: Color,
    pub panel_bg: Color,
    pub active_row_bg: Color,
    pub selection_bg: Color,
    pub accent: Color,
    pub fg: Color,
    pub dim: Color,
    pub red: Color,
    pub yellow: Color,
    pub green: Color,
    pub blue: Color,
    pub magenta: Color,
    pub cyan: Color,
    /// The colour of an agent that needs you: blocked rows, glyphs and the border count.
    /// Named separately from `yellow` because Catppuccin's yellow is a pale cream that
    /// reads as plain text beside `fg`.
    pub attention: Color,
}

impl Theme {
    /// The colour of an agent state, matching the status rail: blocked attention, working
    /// blue, done green, idle dim, unknown magenta. `None` (a space with no agents) is
    /// dim.
    pub fn state_color(&self, state: Option<AgentState>) -> Color {
        match state {
            Some(AgentState::Blocked) => self.attention,
            Some(AgentState::Working) => self.blue,
            Some(AgentState::Done) => self.green,
            Some(AgentState::Idle) => self.dim,
            Some(AgentState::Unknown) => self.magenta,
            None => self.dim,
        }
    }
}

/// A resolved theme plus what the settings overlay needs to say about it.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub theme: Theme,
    /// The theme actually in use — `DEFAULT_NAME` when the configured name was unknown.
    pub name: String,
    /// One line per problem found while resolving (unknown theme name, unparseable
    /// custom colour), shown in the settings overlay.
    pub warnings: Vec<String>,
}

/// The light or dark appearance of the terminal a popup's tmux client is on, as tmux 3.6's
/// `#{client_theme}` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
    /// `unknown`, an empty answer (tmux before 3.6 has no such format), or anything else.
    Unknown,
}

impl Appearance {
    /// Reads what `#{client_theme}` printed.
    pub fn parse(said: &str) -> Appearance {
        match said.trim() {
            "dark" => Appearance::Dark,
            "light" => Appearance::Light,
            _ => Appearance::Unknown,
        }
    }
}

/// The built-in themes that come as a dark and a light pair, dark first. herdr fills an omitted `dark_name` or
/// `light_name` from the sibling of `name` for that mode, and uses `name` itself where it has none; of herdr's
/// pairs taw has this one.
const SIBLINGS: &[(&str, &str)] = &[("catppuccin", "catppuccin-latte")];

/// The theme name for `appearance`, whether `auto_switch` is on or not: `dark_name` or `light_name` when set,
/// else `name`'s sibling for that mode, else `name` itself (so `name = "tokyo-night"` stays tokyo-night on both
/// sides rather than switching to another palette). An unknown appearance is `name`.
pub fn name_for(theme: &crate::config::Theme, appearance: Appearance) -> &str {
    let name = theme.name.as_str();
    match appearance {
        Appearance::Dark => theme.dark_name.as_deref().unwrap_or_else(|| {
            SIBLINGS
                .iter()
                .find(|(_, light)| *light == name)
                .map_or(name, |(dark, _)| dark)
        }),
        Appearance::Light => theme.light_name.as_deref().unwrap_or_else(|| {
            SIBLINGS
                .iter()
                .find(|(dark, _)| *dark == name)
                .map_or(name, |(_, light)| light)
        }),
        Appearance::Unknown => name,
    }
}

/// The theme name drawn: with `auto_switch` on, [`name_for`] the appearance; with it off, always `name`. With
/// nothing set at all the pair is `catppuccin` and `catppuccin-latte`, as herdr's default config lists.
pub fn chosen_name(theme: &crate::config::Theme, appearance: Appearance) -> &str {
    if theme.auto_switch {
        name_for(theme, appearance)
    } else {
        &theme.name
    }
}

/// Resolves `[theme] name` to a built-in palette and applies `[theme.custom]` on top.
///
/// An unknown theme name falls back to [`DEFAULT_NAME`] with a warning; an unknown
/// custom token name or an unparseable colour is warned about and skipped, so one typo
/// cannot leave the popup unusable.
pub fn resolve(name: &str, custom: &BTreeMap<String, String>) -> Resolved {
    let mut warnings = Vec::new();
    let (name, mut theme) = match built_in(name) {
        Some(theme) => (name.to_string(), theme),
        None => {
            warnings.push(format!(
                "unknown [theme] name '{name}', falling back to '{DEFAULT_NAME}'"
            ));
            (
                DEFAULT_NAME.to_string(),
                built_in(DEFAULT_NAME).expect("the default theme is built in"),
            )
        }
    };
    for (token, value) in custom {
        let Some(slot) = token_slot(&mut theme, token) else {
            warnings.push(format!("unknown [theme.custom] token '{token}', ignored"));
            continue;
        };
        match parse_color(value) {
            Some(colour) => *slot = colour,
            None => warnings.push(format!(
                "[theme.custom] {token}: '{value}' is not a colour, ignored"
            )),
        }
    }
    Resolved {
        theme,
        name,
        warnings,
    }
}

/// A mutable handle on one named token of `theme`, or `None` when the name is not one of
/// the fourteen. Keeping the name-to-field mapping in one place is what lets `resolve`
/// apply overrides generically and still warn about typos.
fn token_slot<'a>(theme: &'a mut Theme, token: &str) -> Option<&'a mut Color> {
    Some(match token {
        "sidebar_bg" => &mut theme.sidebar_bg,
        "panel_bg" => &mut theme.panel_bg,
        "active_row_bg" => &mut theme.active_row_bg,
        "selection_bg" => &mut theme.selection_bg,
        "accent" => &mut theme.accent,
        "fg" => &mut theme.fg,
        "dim" => &mut theme.dim,
        "red" => &mut theme.red,
        "yellow" => &mut theme.yellow,
        "green" => &mut theme.green,
        "blue" => &mut theme.blue,
        "magenta" => &mut theme.magenta,
        "cyan" => &mut theme.cyan,
        "attention" => &mut theme.attention,
        _ => return None,
    })
}

/// The fourteen tokens of a built-in palette, in `Theme`'s field order.
type Palette = [&'static str; 14];

fn from_palette(p: &Palette) -> Theme {
    let c = |i: usize| {
        parse_color(p[i]).unwrap_or_else(|| panic!("built-in palette colour '{}' is invalid", p[i]))
    };
    Theme {
        sidebar_bg: c(0),
        panel_bg: c(1),
        active_row_bg: c(2),
        selection_bg: c(3),
        accent: c(4),
        fg: c(5),
        dim: c(6),
        red: c(7),
        yellow: c(8),
        green: c(9),
        blue: c(10),
        magenta: c(11),
        cyan: c(12),
        attention: c(13),
    }
}

/// The built-in palettes. Each row is
/// `[sidebar_bg, panel_bg, active_row_bg, selection_bg, accent, fg, dim, red, yellow,
/// green, blue, magenta, cyan, attention]`, taken from each theme's published palette —
/// and, for `terminal`, from the ANSI names only, so it inherits whatever the host
/// terminal is themed with.
fn built_in(name: &str) -> Option<Theme> {
    let palette: Palette = match name {
        // Catppuccin Mocha: mantle, base, surface0, surface1, blue, text, overlay1,
        // red, yellow, green, blue, mauve, teal, peach.
        "catppuccin" => [
            "#181825", "#1e1e2e", "#313244", "#45475a", "#89b4fa", "#cdd6f4", "#7f849c", "#f38ba8",
            "#f9e2af", "#a6e3a1", "#89b4fa", "#cba6f7", "#94e2d5", "#fab387",
        ],
        // Catppuccin Latte (light), plus its Peach for attention.
        "catppuccin-latte" => [
            "#e6e9ef", "#eff1f5", "#dce0e8", "#ccd0da", "#1e66f5", "#4c4f69", "#8c8fa1", "#d20f39",
            "#df8e1d", "#40a02b", "#1e66f5", "#8839ef", "#179299", "#fe640b",
        ],
        // ANSI only: no rgb values at all, so the host terminal's own palette shows
        // through (`reset` for the backgrounds it would otherwise paint over). No
        // separate ANSI orange exists, so attention reuses yellow here.
        "terminal" => [
            "reset",
            "reset",
            "brightblack",
            "blue",
            "cyan",
            "reset",
            "brightblack",
            "red",
            "yellow",
            "green",
            "blue",
            "magenta",
            "cyan",
            "yellow",
        ],
        "tokyo-night" => [
            "#16161e", "#1a1b26", "#292e42", "#33467c", "#7aa2f7", "#c0caf5", "#565f89", "#f7768e",
            "#e0af68", "#9ece6a", "#7aa2f7", "#bb9af7", "#7dcfff", "#ff9e64",
        ],
        "dracula" => [
            "#21222c", "#282a36", "#44475a", "#6272a4", "#bd93f9", "#f8f8f2", "#6272a4", "#ff5555",
            "#f1fa8c", "#50fa7b", "#6272a4", "#ff79c6", "#8be9fd", "#ffb86c",
        ],
        "nord" => [
            "#2e3440", "#3b4252", "#434c5e", "#4c566a", "#88c0d0", "#d8dee9", "#616e88", "#bf616a",
            "#ebcb8b", "#a3be8c", "#81a1c1", "#b48ead", "#8fbcbb", "#d08770",
        ],
        "gruvbox" => [
            "#1d2021", "#282828", "#3c3836", "#504945", "#fabd2f", "#ebdbb2", "#928374", "#fb4934",
            "#fabd2f", "#b8bb26", "#83a598", "#d3869b", "#8ec07c", "#fe8019",
        ],
        "one-dark" => [
            "#21252b", "#282c34", "#2c313a", "#3e4451", "#61afef", "#abb2bf", "#5c6370", "#e06c75",
            "#e5c07b", "#98c379", "#61afef", "#c678dd", "#56b6c2", "#d19a66",
        ],
        // Solarized dark: base03 sidebar, base02 panel, base01 selection, base0 text,
        // orange attention.
        "solarized" => [
            "#002b36", "#073642", "#0a4552", "#586e75", "#268bd2", "#839496", "#586e75", "#dc322f",
            "#b58900", "#859900", "#268bd2", "#d33682", "#2aa198", "#cb4b16",
        ],
        "kanagawa" => [
            "#16161d", "#1f1f28", "#2a2a37", "#363646", "#7e9cd8", "#dcd7ba", "#727169", "#c34043",
            "#e6c384", "#98bb6c", "#7e9cd8", "#957fb8", "#7aa89f", "#ffa066",
        ],
        // Rose Pine, using its own published ANSI mapping (green = pine, blue = foam,
        // cyan = rose) rather than inventing hues the palette does not have. Its main
        // variant's warm accents (love, gold, rose) are already spoken for above, so
        // attention borrows the Dawn variant's more saturated gold, #ea9d34, rather than
        // repeating the pale one used for yellow.
        "rose-pine" => [
            "#191724", "#1f1d2e", "#26233a", "#403d52", "#c4a7e7", "#e0def4", "#6e6a86", "#eb6f92",
            "#f6c177", "#31748f", "#9ccfd8", "#c4a7e7", "#ebbcba", "#ea9d34",
        ],
        // Vesper. Its signature orange, #ffc799, is already the accent, and its yellow is
        // the pale #e6b99d, so attention takes the palette's other published warm signal,
        // #ff8080, rather than repeating the accent — the ANSI red above is the softer
        // #f5a191, which this does not clash with either.
        "vesper" => [
            "#101010", "#161616", "#232323", "#2a2a2a", "#ffc799", "#ffffff", "#8b8b8b", "#f5a191",
            "#e6b99d", "#90b99f", "#aca1cf", "#e29eca", "#ea83a5", "#ff8080",
        ],
        _ => return None,
    };
    Some(from_palette(&palette))
}

/// Parses one `[theme.custom]` value (or a row token's `fg`): `#rrggbb`/`#rgb`, a named colour (`cyan`,
/// `brightblack`), a 256-colour index, `rgb(r,g,b)`, or `reset` for the terminal's own
/// default.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(inner) = s
        .strip_prefix("rgb(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        // ratatui's own FromStr does not know herdr's `rgb(r,g,b)` spelling.
        let mut parts = inner.split(',');
        let mut next = || parts.next()?.trim().parse::<u8>().ok();
        let (r, g, b) = (next()?, next()?, next()?);
        return parts.next().is_none().then_some(Color::Rgb(r, g, b));
    }
    // `#rgb`, each digit doubled (`#f55` is `#ff5555`), as herdr reads it: ratatui's own
    // parser takes six digits only, and a herdr config writes `fg = "#f55"` as often as
    // `"#ff5555"`.
    if let Some(hex) = s
        .strip_prefix('#')
        .filter(|hex| hex.len() == 3 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        let mut digits = hex
            .bytes()
            .map(|b| char::from(b).to_digit(16).unwrap_or_default() as u8 * 17);
        if let (Some(r), Some(g), Some(b)) = (digits.next(), digits.next(), digits.next()) {
            return Some(Color::Rgb(r, g, b));
        }
    }
    // `Color::from_str` covers `reset`, the named colours (including `bright*`/`grey`
    // spellings), a 0-255 palette index, and `#rrggbb`.
    Color::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme_config(toml: &str) -> crate::config::Theme {
        let cfg: crate::config::Config = toml::from_str(toml).unwrap();
        cfg.theme
    }

    /// With `auto_switch` on, a dark client draws `dark_name`, a light one `light_name`, and one that cannot say
    /// (`unknown`, empty, or tmux before 3.6) draws `name`.
    #[test]
    fn auto_switch_picks_the_name_for_the_clients_appearance() {
        let theme = theme_config(
            "[theme]\nname = \"nord\"\nauto_switch = true\ndark_name = \"dracula\"\nlight_name = \"solarized\"\n",
        );
        assert_eq!(chosen_name(&theme, Appearance::parse("dark")), "dracula");
        assert_eq!(
            chosen_name(&theme, Appearance::parse("light\n")),
            "solarized"
        );
        for said in ["unknown", "", "  ", "blue"] {
            assert_eq!(
                chosen_name(&theme, Appearance::parse(said)),
                "nord",
                "{said:?}"
            );
        }
    }

    /// Without `auto_switch` nothing changes: `name` whatever the client says, the two names ignored.
    #[test]
    fn without_auto_switch_the_name_is_always_drawn() {
        let theme = theme_config(
            "[theme]\nname = \"nord\"\ndark_name = \"dracula\"\nlight_name = \"solarized\"\n",
        );
        for said in ["dark", "light", "unknown", ""] {
            assert_eq!(
                chosen_name(&theme, Appearance::parse(said)),
                "nord",
                "{said:?}"
            );
        }
    }

    /// `auto_switch` alone is enough: with nothing else set the pair is the one herdr's default config lists, read
    /// here from the vendored copy of it.
    #[test]
    fn auto_switch_alone_uses_the_names_herdrs_default_config_lists() {
        let herdr = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("third_party/herdr/reference/default-config.toml"),
        )
        .unwrap();
        let listed = |key: &str| {
            let line = herdr
                .lines()
                .find_map(|l| l.strip_prefix(&format!("# {key} = ")))
                .unwrap_or_else(|| panic!("herdr's default config lists no {key}"));
            line.trim_matches('"').to_string()
        };
        let theme = theme_config("[theme]\nauto_switch = true\n");
        assert_eq!(chosen_name(&theme, Appearance::Dark), listed("dark_name"));
        assert_eq!(chosen_name(&theme, Appearance::Light), listed("light_name"));
        assert_eq!(chosen_name(&theme, Appearance::Unknown), "catppuccin");
        // The two defaults are different palettes, so a switch is something to see.
        let colours = |a| resolve(chosen_name(&theme, a), &BTreeMap::new()).theme;
        assert_ne!(colours(Appearance::Dark), colours(Appearance::Light));
    }

    /// An omitted `dark_name` or `light_name` is `name`'s sibling for that mode where taw has one (catppuccin and
    /// catppuccin-latte, from either side), and `name` itself where it has none, so `name = "tokyo-night"` is
    /// tokyo-night on both sides. A name that is set wins over the sibling, and each key stands alone.
    #[test]
    fn an_omitted_name_is_the_sibling_of_name_or_name_itself() {
        let with = |toml: &str| theme_config(&format!("[theme]\nauto_switch = true\n{toml}"));
        let both = |t: &crate::config::Theme| {
            (
                chosen_name(t, Appearance::Dark).to_string(),
                chosen_name(t, Appearance::Light).to_string(),
            )
        };
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            both(&with("name = \"tokyo-night\"\n")),
            pair("tokyo-night", "tokyo-night")
        );
        assert_eq!(
            both(&with("name = \"catppuccin\"\n")),
            pair("catppuccin", "catppuccin-latte")
        );
        assert_eq!(
            both(&with("name = \"catppuccin-latte\"\n")),
            pair("catppuccin", "catppuccin-latte")
        );
        assert_eq!(
            both(&with("name = \"nord\"\nlight_name = \"solarized\"\n")),
            pair("nord", "solarized")
        );
        assert_eq!(
            both(&with("name = \"nord\"\ndark_name = \"dracula\"\n")),
            pair("dracula", "nord")
        );
        assert_eq!(
            both(&with(
                "name = \"dracula\"\ndark_name = \"nord\"\nlight_name = \"solarized\"\n"
            )),
            pair("nord", "solarized")
        );
    }

    fn custom(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolve_overrides_one_token_and_leaves_the_rest_of_the_palette_alone() {
        let base = resolve("catppuccin", &BTreeMap::new());
        let over = resolve("catppuccin", &custom(&[("accent", "#ff0000")]));
        assert_eq!(over.theme.accent, Color::Rgb(255, 0, 0));
        assert_eq!(over.theme.fg, base.theme.fg);
        assert_eq!(over.theme.sidebar_bg, base.theme.sidebar_bg);
        assert!(over.warnings.is_empty(), "{:?}", over.warnings);
    }

    #[test]
    fn resolve_accepts_every_built_in_name_and_they_are_all_distinct() {
        let mut seen = Vec::new();
        for name in NAMES {
            let r = resolve(name, &BTreeMap::new());
            assert_eq!(&r.name, name);
            assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
            assert!(
                !seen.contains(&r.theme),
                "{name} duplicates another palette"
            );
            seen.push(r.theme);
        }
        assert_eq!(seen.len(), NAMES.len());
    }

    #[test]
    fn resolve_falls_back_to_catppuccin_with_a_warning_on_an_unknown_name() {
        let r = resolve("solarized-midnight-plus", &BTreeMap::new());
        assert_eq!(r.name, DEFAULT_NAME);
        assert_eq!(r.theme, resolve(DEFAULT_NAME, &BTreeMap::new()).theme);
        assert_eq!(r.warnings.len(), 1);
        assert!(
            r.warnings[0].contains("solarized-midnight-plus")
                && r.warnings[0].contains("catppuccin"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn resolve_warns_about_an_unknown_token_and_an_unparseable_colour_and_keeps_going() {
        let r = resolve(
            "catppuccin",
            &custom(&[
                ("accent", "not-a-colour-at-all"),
                ("border_bg", "#ffffff"),
                ("green", "rgb(0, 255, 0)"),
            ]),
        );
        let base = resolve("catppuccin", &BTreeMap::new());
        assert_eq!(
            r.theme.accent, base.theme.accent,
            "bad value must be ignored"
        );
        assert_eq!(
            r.theme.green,
            Color::Rgb(0, 255, 0),
            "good value still applies"
        );
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(
            r.warnings.iter().any(|w| w.contains("border_bg")),
            "{:?}",
            r.warnings
        );
        assert!(
            r.warnings.iter().any(|w| w.contains("accent")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn resolve_maps_reset_to_the_terminal_default_background() {
        let r = resolve("catppuccin", &custom(&[("panel_bg", "reset")]));
        assert_eq!(r.theme.panel_bg, Color::Reset);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn the_terminal_theme_uses_only_ansi_colours_so_it_inherits_the_host_palette() {
        let t = resolve("terminal", &BTreeMap::new()).theme;
        for (token, colour) in [
            ("sidebar_bg", t.sidebar_bg),
            ("panel_bg", t.panel_bg),
            ("fg", t.fg),
            ("accent", t.accent),
            ("red", t.red),
            ("green", t.green),
        ] {
            assert!(
                !matches!(colour, Color::Rgb(..)),
                "{token} must not be a hard-coded rgb value in the terminal theme"
            );
        }
    }

    #[test]
    fn parse_color_reads_hex_named_indexed_rgb_and_reset() {
        assert_eq!(parse_color("#89b4fa"), Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        // The three-digit form doubles each digit: herdr's `fg = "#f55"`.
        assert_eq!(parse_color("#f55"), Some(Color::Rgb(0xff, 0x55, 0x55)));
        assert_eq!(parse_color("#FC0"), Some(Color::Rgb(0xff, 0xcc, 0x00)));
        assert_eq!(parse_color("#fffa"), None, "four digits are no colour");
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("brightblack"), Some(Color::DarkGray));
        assert_eq!(parse_color("42"), Some(Color::Indexed(42)));
        assert_eq!(parse_color("rgb(1,2,3)"), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(
            parse_color("rgb( 10 , 20 , 30 )"),
            Some(Color::Rgb(10, 20, 30))
        );
        assert_eq!(parse_color("reset"), Some(Color::Reset));
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("rgb(1,2)"), None);
        assert_eq!(parse_color("rgb(1,2,300)"), None);
        assert_eq!(parse_color("chartreuse-ish"), None);
    }

    #[test]
    fn state_colours_follow_the_status_rail() {
        let t = resolve("catppuccin", &BTreeMap::new()).theme;
        assert_eq!(t.state_color(Some(AgentState::Blocked)), t.attention);
        assert_eq!(t.state_color(Some(AgentState::Working)), t.blue);
        assert_eq!(t.state_color(Some(AgentState::Done)), t.green);
        assert_eq!(t.state_color(Some(AgentState::Idle)), t.dim);
        assert_eq!(t.state_color(Some(AgentState::Unknown)), t.magenta);
        assert_eq!(t.state_color(None), t.dim);
    }

    #[test]
    fn blocked_is_drawn_in_the_attention_colour_not_yellow() {
        let t = built_in("catppuccin").unwrap();
        assert_eq!(t.state_color(Some(AgentState::Blocked)), t.attention);
        assert_ne!(
            t.attention, t.yellow,
            "the palette's yellow is a pale cream; attention must stand apart from it"
        );
        assert_eq!(t.attention, parse_color("#fab387").unwrap());
        let latte = built_in("catppuccin-latte").unwrap();
        assert_eq!(latte.attention, parse_color("#fe640b").unwrap());
    }

    #[test]
    fn attention_is_a_custom_token() {
        let mut custom = BTreeMap::new();
        custom.insert("attention".to_string(), "#ff0000".to_string());
        let resolved = resolve("catppuccin", &custom);
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert_eq!(resolved.theme.attention, parse_color("#ff0000").unwrap());
    }

    #[test]
    fn every_built_in_theme_has_an_attention_colour() {
        for name in NAMES {
            let t = built_in(name).unwrap();
            // The `terminal` theme is ANSI-only and keeps yellow; every rgb theme picks an
            // orange that is not its yellow.
            if *name != "terminal" {
                assert_ne!(t.attention, t.yellow, "{name}");
            }
            // And never the accent either: the accent is what every session name and the
            // query are drawn in, so an attention colour equal to it would make a blocked
            // agent look like ordinary chrome.
            assert_ne!(t.attention, t.accent, "{name}");
        }
    }
}
