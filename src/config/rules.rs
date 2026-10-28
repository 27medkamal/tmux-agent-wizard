// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! A sidebar row token's `rules`: up to sixteen, in order, each one a condition on the
//! token's text and what to do to the token when it holds.
//!
//! ```toml
//! rows = [[{ token = "$load", fg = "#fff", rules = [{ gt = 80, fg = "#f55", bold = true },
//!                                                   { gt = 50, fg = "#fc0" }] }]]
//! ```
//!
//! The first rule that matches wins. Its `fg`, `bold` and `dim` are laid over the token's own
//! (what it leaves out is the token's); `hide = true` takes the token out of the row, and its
//! separator with it, so a row left with no token is not drawn. A rule with no style still
//! ends the search and keeps the token as it is. There are five conditions, and a rule has
//! exactly one: `equals`, `contains` and `starts_with` compare text (exactly, or with ASCII
//! case folded under `ignore_case = true`), `gt` and `lt` compare a finite number strictly,
//! and the token's text has to be one, all of it, with no space, unit or `NaN` around it.

use std::fmt;

use serde::Deserialize;

use crate::tui::theme;

/// What a token is drawn with, before the theme fills in the colour it leaves open: the
/// `fg`, `bold` and `dim` of the token itself, or of the rule that matched it laid over them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenStyle {
    pub fg: Option<String>,
    pub bold: Option<bool>,
    pub dim: Option<bool>,
}

/// One entry of a token's `rules`. Read from a table with exactly one condition; anything
/// else is refused when the config loads (see [`RawRule`]).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawRule")]
pub struct TokenRule {
    condition: Condition,
    ignore_case: bool,
    style: TokenStyle,
    hide: Option<bool>,
}

// Deserialization rejects non-finite thresholds, so equality is reflexive.
impl Eq for TokenRule {}

#[derive(Clone, Debug, PartialEq)]
enum Condition {
    Equals(String),
    Contains(String),
    StartsWith(String),
    GreaterThan(f64),
    LessThan(f64),
}

/// A rule as written: every field optional, unknown ones refused, and [`TokenRule`]'s
/// `TryFrom` the one place that says what a well-formed rule is.
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRule {
    equals: Option<String>,
    contains: Option<String>,
    starts_with: Option<String>,
    gt: Option<f64>,
    lt: Option<f64>,
    ignore_case: Option<bool>,
    fg: Option<String>,
    bold: Option<bool>,
    dim: Option<bool>,
    hide: Option<bool>,
}

impl TryFrom<RawRule> for TokenRule {
    type Error = String;

    fn try_from(raw: RawRule) -> Result<Self, Self::Error> {
        let count = [
            raw.equals.is_some(),
            raw.contains.is_some(),
            raw.starts_with.is_some(),
            raw.gt.is_some(),
            raw.lt.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if count != 1 {
            return Err(
                "sidebar rule requires exactly one of equals, contains, starts_with, gt, lt".into(),
            );
        }
        let condition = if let Some(value) = raw.equals {
            Condition::Equals(value)
        } else if let Some(value) = raw.contains {
            Condition::Contains(value)
        } else if let Some(value) = raw.starts_with {
            Condition::StartsWith(value)
        } else {
            let (value, greater) = match (raw.gt, raw.lt) {
                (Some(value), _) => (value, true),
                (_, Some(value)) => (value, false),
                _ => unreachable!("validated condition count"),
            };
            if !value.is_finite() {
                return Err("sidebar numeric rule threshold must be finite".into());
            }
            if raw.ignore_case.is_some() {
                return Err("ignore_case applies only to sidebar text conditions".into());
            }
            if greater {
                Condition::GreaterThan(value)
            } else {
                Condition::LessThan(value)
            }
        };
        if let Some(fg) = &raw.fg
            && theme::parse_color(fg).is_none()
        {
            return Err(format!("sidebar rule fg is not a colour: {fg}"));
        }
        Ok(Self {
            condition,
            ignore_case: raw.ignore_case.unwrap_or(false),
            hide: raw.hide,
            style: TokenStyle {
                fg: raw.fg,
                bold: raw.bold,
                dim: raw.dim,
            },
        })
    }
}

/// The rule as the config file would write it, for the options overlay.
impl fmt::Display for TokenRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = quoted;
        let number = |value: f64| toml::Value::Float(value).to_string();
        let mut parts = vec![match &self.condition {
            Condition::Equals(value) => format!("equals = {}", text(value)),
            Condition::Contains(value) => format!("contains = {}", text(value)),
            Condition::StartsWith(value) => format!("starts_with = {}", text(value)),
            Condition::GreaterThan(value) => format!("gt = {}", number(*value)),
            Condition::LessThan(value) => format!("lt = {}", number(*value)),
        }];
        if self.ignore_case {
            parts.push("ignore_case = true".to_string());
        }
        if let Some(fg) = &self.style.fg {
            parts.push(format!("fg = {}", text(fg)));
        }
        if let Some(bold) = self.style.bold {
            parts.push(format!("bold = {bold}"));
        }
        if let Some(dim) = self.style.dim {
            parts.push(format!("dim = {dim}"));
        }
        if let Some(hide) = self.hide {
            parts.push(format!("hide = {hide}"));
        }
        write!(f, "{{ {} }}", parts.join(", "))
    }
}

/// `value` as a TOML basic string: quoted, with what TOML will not take raw escaped.
fn quoted(value: &str) -> String {
    let mut out = String::from('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl TokenRule {
    /// Whether `value`, the token's text, meets the condition. `numeric` holds what the
    /// text parses to once one numeric rule has asked, so a list of them parses it once.
    fn matches(&self, value: &str, numeric: &mut Option<Option<f64>>) -> bool {
        match &self.condition {
            Condition::Equals(expected) => {
                if self.ignore_case {
                    value.eq_ignore_ascii_case(expected)
                } else {
                    value == expected
                }
            }
            Condition::StartsWith(expected) => {
                if self.ignore_case {
                    value
                        .as_bytes()
                        .get(..expected.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(expected.as_bytes()))
                } else {
                    value.starts_with(expected)
                }
            }
            Condition::Contains(expected) => {
                if self.ignore_case {
                    expected.is_empty()
                        || value
                            .as_bytes()
                            .windows(expected.len())
                            .any(|part| part.eq_ignore_ascii_case(expected.as_bytes()))
                } else {
                    value.contains(expected)
                }
            }
            Condition::GreaterThan(threshold) | Condition::LessThan(threshold) => {
                let parsed = numeric.get_or_insert_with(|| {
                    value
                        .parse::<f64>()
                        .ok()
                        .filter(|number| number.is_finite())
                });
                parsed.is_some_and(|number| match self.condition {
                    Condition::GreaterThan(_) => number > *threshold,
                    _ => number < *threshold,
                })
            }
        }
    }
}

/// What a token with `rules` is drawn with for `value`, its text: `base` with the first
/// matching rule's fields laid over it, or `base` alone when none matches. `None` when the
/// rule that matched hides the token.
pub fn matching_style(rules: &[TokenRule], base: TokenStyle, value: &str) -> Option<TokenStyle> {
    let mut numeric = None;
    for rule in rules {
        if rule.matches(value, &mut numeric) {
            if rule.hide == Some(true) {
                return None;
            }
            return Some(TokenStyle {
                fg: rule.style.fg.clone().or(base.fg),
                bold: rule.style.bold.or(base.bold),
                dim: rule.style.dim.or(base.dim),
            });
        }
    }
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Follows herdr's src/config/sidebar/rules.rs tests.
    #[test]
    fn string_conditions_use_exact_case_or_ascii_folding() {
        for (condition, yes, no) in [
            ("equals = 'Local'", "Local", "Localhost"),
            ("contains = 'Local'", "myLocalbox", "remote"),
            ("starts_with = 'Local'", "Localhost", "myLocal"),
        ] {
            let rule: TokenRule = toml::from_str(condition).unwrap();
            assert!(rule.matches(yes, &mut None));
            assert!(!rule.matches(no, &mut None));
            assert!(!rule.matches(&yes.to_ascii_lowercase(), &mut None));
            let folded: TokenRule =
                toml::from_str(&format!("{condition}\nignore_case = true")).unwrap();
            assert!(folded.matches(&yes.to_ascii_lowercase(), &mut None));
        }
        for condition in ["equals", "contains", "starts_with"] {
            let rule: TokenRule =
                toml::from_str(&format!("{condition} = 'ÉA'\nignore_case = true")).unwrap();
            assert!(rule.matches("Éa", &mut None));
            assert!(!rule.matches("éa", &mut None));
        }
        let empty: TokenRule = toml::from_str("contains = ''\nignore_case = true").unwrap();
        assert!(empty.matches("", &mut None));
    }

    #[test]
    fn numeric_conditions_require_full_finite_numbers_and_strict_comparison() {
        for (condition, yes, no) in [
            ("gt = 80", ["90", "8.1e1", "+90"], "70"),
            ("lt = 80", ["70", "7.9e1", "-90"], "90"),
        ] {
            let rule: TokenRule = toml::from_str(condition).unwrap();
            for value in yes {
                assert!(rule.matches(value, &mut None), "{condition}: {value}");
            }
            for value in [
                no, "80", "90%", " 90", "90 ", "", "NaN", "inf", "-inf", "1e999",
            ] {
                assert!(!rule.matches(value, &mut None), "{condition}: {value}");
            }
        }
    }

    /// herdr's docs: empty `equals` matches only an empty value, while empty `contains` or
    /// `starts_with` matches any value.
    #[test]
    fn empty_strings_follow_ordinary_string_matching() {
        let equals: TokenRule = toml::from_str("equals = ''").unwrap();
        assert!(equals.matches("", &mut None));
        assert!(!equals.matches("x", &mut None));
        for condition in ["contains = ''", "starts_with = ''"] {
            let rule: TokenRule = toml::from_str(condition).unwrap();
            assert!(rule.matches("x", &mut None), "{condition}");
            assert!(rule.matches("", &mut None), "{condition}");
        }
    }

    fn style(fg: Option<&str>, bold: Option<bool>, dim: Option<bool>) -> TokenStyle {
        TokenStyle {
            fg: fg.map(str::to_string),
            bold,
            dim,
        }
    }

    #[test]
    fn the_first_matching_rule_wins_and_unspecified_fields_inherit() {
        let rules: Vec<TokenRule> = [
            "gt = 80\nfg = '#f00'",
            "gt = 50\nfg = '#0f0'\nbold = false",
            "contains = 'x'\ndim = false",
        ]
        .into_iter()
        .map(|rule| toml::from_str(rule).unwrap())
        .collect();
        let base = style(Some("#00f"), Some(true), Some(true));

        assert_eq!(
            matching_style(&rules, base.clone(), "90"),
            Some(style(Some("#f00"), Some(true), Some(true)))
        );
        assert_eq!(
            matching_style(&rules, base.clone(), "60"),
            Some(style(Some("#0f0"), Some(false), Some(true)))
        );
        assert_eq!(
            matching_style(&rules, base.clone(), "xyz"),
            Some(style(Some("#00f"), Some(true), Some(false)))
        );
        assert_eq!(matching_style(&rules, base.clone(), "10"), Some(base));
    }

    #[test]
    fn hide_removes_the_token_only_when_true_and_a_rule_with_no_overrides_still_stops_the_search() {
        let hide: TokenRule = toml::from_str("equals = 'Local'\nhide = true").unwrap();
        let keep: TokenRule = toml::from_str("equals = 'Local'\nhide = false").unwrap();
        let plain: TokenRule = toml::from_str("equals = 'Local'").unwrap();
        let later: TokenRule = toml::from_str("contains = 'Loc'\nfg = '#f00'").unwrap();
        let base = style(Some("#fff"), None, None);

        assert_eq!(
            matching_style(std::slice::from_ref(&hide), base.clone(), "Local"),
            None
        );
        assert_eq!(
            matching_style(std::slice::from_ref(&hide), base.clone(), "Localhost"),
            Some(base.clone())
        );
        assert_eq!(
            matching_style(std::slice::from_ref(&keep), base.clone(), "Local"),
            Some(base.clone())
        );
        assert_eq!(
            matching_style(&[plain, later], base.clone(), "Local"),
            Some(base),
            "a match with nothing to change is still the first match"
        );
    }

    #[test]
    fn a_rule_reads_back_as_the_config_writes_it() {
        for written in [
            r##"{ equals = "Local", ignore_case = true, fg = "#f55", bold = true, dim = false, hide = true }"##,
            r#"{ contains = "say \"hi\"" }"#,
            r#"{ starts_with = "a\\b", dim = true }"#,
            r#"{ gt = 80.5, bold = true }"#,
            r#"{ lt = -3.0 }"#,
        ] {
            let rule: TokenRule =
                toml::from_str(&written[2..written.len() - 2].replace(", ", "\n"))
                    .unwrap_or_else(|e| panic!("{written}: {e}"));
            assert_eq!(rule.to_string(), written);
        }
    }
}
