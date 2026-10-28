//! tmux version detection and feature gates.

/// A parsed tmux version, e.g. `3.6b` or (for a `next-X.Y` development build) `3.8 ' '`.
///
/// `patch` is the trailing release-letter (`'a'`, `'b'`, …) or `' '` when the version
/// string had none (development/`next-` builds, or a bare `X.Y`).
#[derive(PartialEq, PartialOrd, Debug, Clone, Copy)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: char,
}

impl TmuxVersion {
    /// True when this version is at least `major.minor` (patch letter is ignored).
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }
}

/// Parses tmux's `-V` output, e.g. `"tmux 3.6b"` or `"tmux next-3.8"`.
///
/// Returns `None` if the string is not in a recognised `tmux <ver>` shape.
pub fn parse(s: &str) -> Option<TmuxVersion> {
    let rest = s.trim().strip_prefix("tmux ")?;
    let rest = rest.strip_prefix("next-").unwrap_or(rest);
    let (major_str, rest) = split_leading_digits(rest)?;
    let rest = rest.strip_prefix('.')?;
    let (minor_str, rest) = split_leading_digits(rest)?;
    let major: u32 = major_str.parse().ok()?;
    let minor: u32 = minor_str.parse().ok()?;
    let patch = rest.chars().next().unwrap_or(' ');
    Some(TmuxVersion {
        major,
        minor,
        patch,
    })
}

/// Splits the leading run of ASCII digits off `s`, returning `(digits, rest)`.
/// `None` if `s` does not start with a digit.
fn split_leading_digits(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        None
    } else {
        Some((&s[..end], &s[end..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_release_and_dev_versions() {
        assert_eq!(
            parse("tmux 3.6b"),
            Some(TmuxVersion {
                major: 3,
                minor: 6,
                patch: 'b'
            })
        );
        assert_eq!(
            parse("tmux 3.2a"),
            Some(TmuxVersion {
                major: 3,
                minor: 2,
                patch: 'a'
            })
        );
        assert_eq!(
            parse("tmux next-3.8"),
            Some(TmuxVersion {
                major: 3,
                minor: 8,
                patch: ' '
            })
        );
        assert!(parse("tmux 3.6b").unwrap().at_least(3, 2));
        assert!(!parse("tmux 3.0a").unwrap().at_least(3, 2));
    }
}
