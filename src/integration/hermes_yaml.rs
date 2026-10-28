// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! The one YAML edit taw makes: adding or removing a plugin name in `plugins.enabled`
//! inside Hermes Agent's `config.yaml`.
//!
//! Follows herdr's `src/integration/config_edit.rs`
//! (`update_hermes_enabled_plugin` and its `yaml_*` helpers). Differences: the plugin
//! name is a parameter rather than a herdr constant, and the helpers are private to this
//! module.
//!
//! # Why not a YAML library
//!
//! `config.yaml` belongs to the user: it carries their comments, their key order, their
//! quoting and their indentation. Parsing it into a document model and re-emitting it
//! would rewrite all of that, and taw's contract for every agent config it touches is a
//! byte-for-byte revert. So this edits *lines*, exactly as herdr does — it finds the
//! `plugins.enabled` list in whichever of its four legal spellings the file uses and
//! inserts or removes one line, leaving every other byte alone. Anything it does not
//! recognise it declines to change.

/// Add `plugin` to `plugins.enabled` in `content`, returning `content` unchanged if it is
/// already there.
pub(super) fn enable(content: &str, plugin: &str) -> String {
    update(content, plugin, true)
}

/// Whether [`enable`] can add a plugin to `content` without breaking it: `Err` says what in the
/// file it does not know how to edit, and where. The line edit knows `plugins` as an empty key,
/// an inline list, a list directly under it, or a mapping at two spaces with an `enabled` list
/// (block or inline). Anything else (a flow mapping, a null, a quoted key, another indent, a list
/// at the key's own indent, an `enabled` that is not a list) would get a second `plugins` key or
/// a line out of place, and Hermes would lose the user's plugins or refuse the file.
pub(super) fn check(content: &str) -> Result<(), String> {
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let spelled_quoted = |line: &String| {
        let trimmed = line.trim_end();
        ["\"plugins\"", "'plugins'"].iter().any(|quoted| {
            trimmed
                .strip_prefix(quoted)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
    };
    if let Some(index) = lines.iter().position(spelled_quoted) {
        return Err(format!(
            "line {} spells the plugins key in quotes, which taw's edit does not read",
            index + 1
        ));
    }
    let Some(plugins_index) = top_level_key_index(&lines, "plugins") else {
        return Ok(());
    };
    let line_no = |index: usize| index + 1;
    let value = key_value_at_indent(&lines[plugins_index], 0, "plugins")
        .map(strip_inline_comment)
        .unwrap_or("");
    if !value.is_empty() {
        return match flow_sequence_items(value) {
            Some(_) => Ok(()),
            None => Err(format!(
                "plugins on line {} is not a list that taw's edit can add to",
                line_no(plugins_index)
            )),
        };
    }
    let end = next_top_level_key_index(&lines, plugins_index + 1).unwrap_or(lines.len());
    let first = (plugins_index + 1..end).find(|index| indent(&lines[*index]).is_some());
    if let Some(first) = first
        && indent(&lines[first]) != Some(2)
    {
        return Err(format!(
            "plugins is indented in a way taw's edit does not read (line {}); taw reads its \
             entries at two spaces",
            line_no(first)
        ));
    }
    let enabled =
        (plugins_index + 1..end).find(|index| key_at_indent(&lines[*index], 2) == Some("enabled"));
    if let Some(enabled) = enabled {
        let value = key_value_at_indent(&lines[enabled], 2, "enabled")
            .map(strip_inline_comment)
            .unwrap_or("");
        if !value.is_empty() && flow_sequence_items(value).is_none() {
            return Err(format!(
                "plugins.enabled on line {} is not a list that taw's edit can add to",
                line_no(enabled)
            ));
        }
    }
    Ok(())
}

/// `content` without the lines an install of an older taw added where they left `config.yaml`
/// in a shape Hermes cannot read; `None` when there are none. The older line edit did not
/// check what it was editing, and these are the three results taw can recognise without a YAML
/// parser, because it knows exactly which lines it wrote and where:
///
/// - `plugins: null` (or any value that is not a list) followed by the `  enabled:` and
///   `    - <plugin>` lines the edit inserted under it, which is a scalar with an indented block
///   after it;
/// - `enabled: null` (or any value that is not a list) followed by the `    - <plugin>` item
///   inserted under it;
/// - a quoted `"plugins":` key, with the `plugins:`, `  enabled:` and `    - <plugin>` block the
///   edit appended at the end of the file, which hides the user's own plugins behind a second key.
///
/// Anything else the old edit could have broken (a different indent, a list at the key's own
/// indent) leaves lines that look the same as the user's own, so taw cannot say they are its.
pub(super) fn repair(content: &str, plugin: &str) -> Option<String> {
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    let item = format!("    - {plugin}");
    let is =
        |index: usize, want: &str| lines.get(index).is_some_and(|line| line.trim_end() == want);
    let not_a_list = |value: &str| !value.is_empty() && flow_sequence_items(value).is_none();
    let quoted = lines.iter().any(|line| {
        ["\"plugins\"", "'plugins'"].iter().any(|quoted| {
            line.trim_end()
                .strip_prefix(quoted)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
    });
    let remove: Vec<usize> = if quoted {
        let last = lines.len().checked_sub(3)?;
        if top_level_key_index(&lines, "plugins") == Some(last)
            && is(last, "plugins:")
            && is(last + 1, "  enabled:")
            && is(last + 2, &item)
        {
            vec![last, last + 1, last + 2]
        } else {
            return None;
        }
    } else {
        let plugins_index = top_level_key_index(&lines, "plugins")?;
        let value = key_value_at_indent(&lines[plugins_index], 0, "plugins")
            .map(strip_inline_comment)
            .unwrap_or("");
        if not_a_list(value) {
            if is(plugins_index + 1, "  enabled:") && is(plugins_index + 2, &item) {
                vec![plugins_index + 1, plugins_index + 2]
            } else {
                return None;
            }
        } else {
            let end = next_top_level_key_index(&lines, plugins_index + 1).unwrap_or(lines.len());
            let enabled = (plugins_index + 1..end)
                .find(|index| key_at_indent(&lines[*index], 2) == Some("enabled"))?;
            let value = key_value_at_indent(&lines[enabled], 2, "enabled")
                .map(strip_inline_comment)
                .unwrap_or("");
            if not_a_list(value) && is(enabled + 1, &item) {
                vec![enabled + 1]
            } else {
                return None;
            }
        }
    };
    let kept: Vec<String> = lines
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !remove.contains(index))
        .map(|(_, line)| line)
        .collect();
    Some(join(kept, content.ends_with('\n')))
}

/// Remove `plugin` from `plugins.enabled` in `content`, returning `content` unchanged if it
/// is not there.
pub(super) fn disable(content: &str, plugin: &str) -> String {
    update(content, plugin, false)
}

/// Rewrite `content`'s `plugins` block into one canonical spelling, for comparing two
/// versions of a `config.yaml` without being fooled by a difference that is not a change:
/// an inline `plugins: [a, b]` becomes the equivalent block list, and a block holding no
/// plugins at all — `plugins:`, `plugins: []`, `enabled:`, `enabled: []` and nothing else
/// — is dropped entirely.
///
/// [`disable`] is not quite the inverse of [`enable`], because herdr's line-based edit
/// cannot un-do everything it does: enabling into a file with no `plugins` key appends the
/// whole three-line block, enabling into `enabled: []` expands that line into a block
/// list, and enabling into an inline `plugins: [a]` rewrites it as a block list — while
/// disabling only ever removes the one list item it added.
///
/// Rather than guess which spelling to restore — and risk rewriting a `plugins` key the
/// user wrote — this is applied to *both* sides of the "did anything else change since
/// install?" comparison in [`super::uninstall_text_edit`], and to neither side of what is
/// written. An otherwise-untouched `config.yaml` therefore reverts to its exact original
/// bytes, whichever spelling those were, and a file that really did change keeps its own.
pub(super) fn normalize_for_compare(content: &str) -> String {
    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(start) = top_level_key_index(&lines, "plugins") else {
        return content.to_string();
    };
    let mut end = next_top_level_key_index(&lines, start + 1).unwrap_or(lines.len());

    // An inline sequence and the block list it is equivalent to compare as the same list —
    // whichever of the two keys an edit stages it under.
    if let Some(items) =
        key_value_at_indent(&lines[start], 0, "plugins").and_then(flow_sequence_items)
    {
        let comment = inline_comment(&lines[start]);
        let replacement = flat_plugin_lines(&items, comment);
        let grew = replacement.len();
        lines.splice(start..end, replacement);
        end = start + grew;
    } else if let Some(enabled_index) = lines[start + 1..end]
        .iter()
        .position(|line| key_at_indent(line, 2) == Some("enabled"))
        .map(|offset| start + 1 + offset)
        && let Some(items) =
            key_value_at_indent(&lines[enabled_index], 2, "enabled").and_then(flow_sequence_items)
    {
        let comment = inline_comment(&lines[enabled_index]);
        let replacement = enabled_plugin_lines(&items, comment);
        let grew = replacement.len();
        lines.splice(enabled_index..enabled_index + 1, replacement);
        end += grew - 1;
    }

    let is_scaffold = |line: &str| {
        matches!(
            line.trim(),
            "plugins:" | "plugins: []" | "enabled:" | "enabled: []"
        )
    };
    if !lines[start..end].iter().all(|line| is_scaffold(line)) {
        return join(lines, trailing_newline);
    }

    let mut kept: Vec<String> = lines[..start].to_vec();
    kept.extend_from_slice(&lines[end..]);
    if kept.is_empty() {
        return String::new();
    }
    join(kept, trailing_newline)
}

fn update(content: &str, plugin: &str, enabled: bool) -> String {
    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();

    let Some(plugins_index) = top_level_key_index(&lines, "plugins") else {
        // No `plugins:` key at all: append the whole block, or — when disabling — leave
        // a file that never mentioned us exactly as it is.
        if !enabled {
            return content.to_string();
        }
        let mut result = content.trim_end_matches('\n').to_string();
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&format!("plugins:\n  enabled:\n    - {plugin}\n"));
        return result;
    };

    let plugins_end = next_top_level_key_index(&lines, plugins_index + 1).unwrap_or(lines.len());
    let inline_items =
        key_value_at_indent(&lines[plugins_index], 0, "plugins").and_then(flow_sequence_items);
    let enabled_index = lines[plugins_index + 1..plugins_end]
        .iter()
        .position(|line| key_at_indent(line, 2) == Some("enabled"))
        .map(|offset| plugins_index + 1 + offset);
    let flat_list_start = lines[plugins_index + 1..plugins_end]
        .iter()
        .position(|line| list_item_value_at_indent(line, 2).is_some())
        .map(|offset| plugins_index + 1 + offset);

    // Shape 1: `plugins:` with a nested `enabled:` key.
    if let Some(enabled_index) = enabled_index {
        // `enabled:` itself an inline flow sequence, `enabled: [a, "b"]` — same treatment
        // as Shape 2 below, one indent level in: edit the item list, then rewrite it as a
        // block list (quoting and any inline comment carried over), never left inline.
        if let Some(mut items) =
            key_value_at_indent(&lines[enabled_index], 2, "enabled").and_then(flow_sequence_items)
        {
            let existing = items.iter().position(|item| scalar_value(item) == plugin);
            match (enabled, existing) {
                (true, Some(_)) | (false, None) => return content.to_string(),
                (true, None) => items.insert(0, plugin.to_string()),
                (false, Some(index)) => {
                    items.remove(index);
                }
            }
            let comment = inline_comment(&lines[enabled_index]);
            let replacement = enabled_plugin_lines(&items, comment);
            lines.splice(enabled_index..enabled_index + 1, replacement);
            return join(lines, trailing_newline);
        }

        let list_start = enabled_index + 1;
        let list_end = lines[list_start..plugins_end]
            .iter()
            .position(|line| indent(line).is_some_and(|i| i <= 2) && key_name(line).is_some())
            .map(|offset| list_start + offset)
            .unwrap_or(plugins_end);
        let existing = lines[list_start..list_end]
            .iter()
            .position(|line| list_item_matches(line, plugin))
            .map(|offset| list_start + offset);

        match (enabled, existing) {
            (true, Some(_)) | (false, None) => return content.to_string(),
            (true, None) => lines.insert(list_start, format!("    - {plugin}")),
            (false, Some(index)) => {
                lines.remove(index);
            }
        }
        return join(lines, trailing_newline);
    }

    // Shape 2: an inline flow sequence, `plugins: [a, b]`.
    if let Some(mut items) = inline_items {
        let existing = items.iter().position(|item| scalar_value(item) == plugin);
        match (enabled, existing) {
            (true, Some(_)) | (false, None) => return content.to_string(),
            (true, None) => items.insert(0, plugin.to_string()),
            (false, Some(index)) => {
                items.remove(index);
            }
        }
        let comment = inline_comment(&lines[plugins_index]);
        let replacement = flat_plugin_lines(&items, comment);
        lines.splice(plugins_index..plugins_end, replacement);
        return join(lines, trailing_newline);
    }

    // Shape 3: a flat block list directly under `plugins:`.
    if let Some(flat_list_start) = flat_list_start {
        let existing = lines[plugins_index + 1..plugins_end]
            .iter()
            .position(|line| list_item_matches_at_indent(line, 2, plugin))
            .map(|offset| plugins_index + 1 + offset);

        match (enabled, existing) {
            (true, Some(_)) | (false, None) => return content.to_string(),
            (true, None) => lines.insert(flat_list_start, format!("  - {plugin}")),
            (false, Some(index)) => {
                lines.remove(index);
            }
        }
        return join(lines, trailing_newline);
    }

    // Shape 4: `plugins:` with nothing usable under it — add the nested list.
    if enabled {
        lines.insert(plugins_index + 1, "  enabled:".to_string());
        lines.insert(plugins_index + 2, format!("    - {plugin}"));
        return join(lines, trailing_newline);
    }

    content.to_string()
}

fn flat_plugin_lines(items: &[String], comment: Option<&str>) -> Vec<String> {
    if items.is_empty() {
        return vec![with_inline_comment("plugins: []".to_string(), comment)];
    }
    let mut lines = vec![with_inline_comment("plugins:".to_string(), comment)];
    lines.extend(items.iter().map(|item| format!("  - {item}")));
    lines
}

/// [`flat_plugin_lines`] for the nested `enabled:` key one indent level in.
fn enabled_plugin_lines(items: &[String], comment: Option<&str>) -> Vec<String> {
    if items.is_empty() {
        return vec![with_inline_comment("  enabled: []".to_string(), comment)];
    }
    let mut lines = vec![with_inline_comment("  enabled:".to_string(), comment)];
    lines.extend(items.iter().map(|item| format!("    - {item}")));
    lines
}

/// Appends `comment` (as returned by [`inline_comment`], `#` and all) to `line` with a
/// single space before it — the exact inverse of stripping it back off with
/// [`strip_inline_comment`].
fn with_inline_comment(mut line: String, comment: Option<&str>) -> String {
    if let Some(comment) = comment {
        line.push(' ');
        line.push_str(comment.trim_end());
    }
    line
}

fn top_level_key_index(lines: &[String], key: &str) -> Option<usize> {
    lines
        .iter()
        .position(|line| key_at_indent(line, 0) == Some(key))
}

fn next_top_level_key_index(lines: &[String], start: usize) -> Option<usize> {
    lines[start..]
        .iter()
        .position(|line| indent(line) == Some(0) && key_name(line).is_some())
        .map(|offset| start + offset)
}

fn key_at_indent(line: &str, want: usize) -> Option<&str> {
    (indent(line)? == want).then(|| key_name(line))?
}

fn key_value_at_indent<'a>(line: &'a str, want: usize, key: &str) -> Option<&'a str> {
    if indent(line)? != want {
        return None;
    }
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let (line_key, value) = trimmed.split_once(':')?;
    (line_key.trim() == key).then_some(value.trim())
}

fn key_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let (key, _) = trimmed.split_once(':')?;
    let key = key.trim();
    (!key.is_empty()).then_some(key)
}

fn indent(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    Some(line.len() - trimmed.len())
}

fn list_item_value(line: &str) -> Option<&str> {
    line.trim().strip_prefix("- ").map(str::trim)
}

fn list_item_matches(line: &str, value: &str) -> bool {
    list_item_value(line).is_some_and(|item| scalar_value(item) == value)
}

fn list_item_value_at_indent(line: &str, want: usize) -> Option<&str> {
    (indent(line)? == want).then(|| list_item_value(line))?
}

fn list_item_matches_at_indent(line: &str, want: usize, value: &str) -> bool {
    list_item_value_at_indent(line, want).is_some_and(|item| scalar_value(item) == value)
}

/// Splits an inline `[a, "b", 'c']` flow sequence into its raw item text — quoting and
/// all, exactly as written — so re-serializing an edited list (see [`flat_plugin_lines`],
/// [`enabled_plugin_lines`]) keeps every *other* item's original spelling. Compare items
/// with [`scalar_value`], not directly: `"b"` and `b` name the same plugin.
fn flow_sequence_items(value: &str) -> Option<Vec<String>> {
    let value = strip_inline_comment(value).trim();
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }

    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in inner.chars() {
        if let Some(quote_char) = quote {
            current.push(ch);
            if quote_char == '"' && ch == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if ch == quote_char && !escaped {
                quote = None;
            }
            escaped = false;
            continue;
        }

        match ch {
            '"' | '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            ',' => {
                items.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    // An unterminated quote means this is not a shape we understand; decline to edit.
    if quote.is_some() {
        return None;
    }

    items.push(current.trim().to_string());
    Some(items)
}

fn scalar_value(value: &str) -> String {
    let value = strip_inline_comment(value).trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let quoted = (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'');
        if quoted {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn strip_inline_comment(value: &str) -> &str {
    match inline_comment(value) {
        Some(comment) => value[..value.len() - comment.len()].trim_end(),
        None => value,
    }
}

/// The trailing `# ...` comment on a YAML line, `#` and all — a `#` inside a quoted
/// string does not start one, and it must be preceded by whitespace (or be the whole
/// value) the same as YAML's own rule, so `http://x` is not misread as a comment.
/// [`with_inline_comment`] is the exact inverse: it appends what this returns.
fn inline_comment(value: &str) -> Option<&str> {
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in value.char_indices() {
        if let Some(quote_char) = quote {
            if quote_char == '"' && ch == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if ch == quote_char && !escaped {
                quote = None;
            }
            escaped = false;
            continue;
        }

        match ch {
            '"' | '\'' => quote = Some(ch),
            '#' if index == 0 || value[..index].ends_with(char::is_whitespace) => {
                return Some(&value[index..]);
            }
            _ => {}
        }
    }

    None
}

fn join(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut result = lines.join("\n");
    if trailing_newline || result.is_empty() {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN: &str = "taw-agent-state";

    /// Every shape `enable` handles, and that `disable` undoes each one — exactly where it
    /// can, and up to an empty scaffold where herdr's algorithm cannot (which is what
    /// `normalize_for_compare` exists to absorb).
    #[test]
    fn every_plugins_shape_round_trips() {
        for original in [
            // no plugins key at all
            "",
            "model: sonnet\n",
            // nested enabled list
            "plugins:\n  enabled:\n    - other\n",
            "plugins:\n  enabled: []\n",
            // nested enabled list, with a key after it inside plugins
            "plugins:\n  enabled:\n    - other\n  disabled:\n    - nope\n",
            // flat block list
            "plugins:\n  - other\n",
            // inline flow sequence
            "plugins: [other]\n",
            "plugins: []\n",
            // nested enabled list, as an inline flow sequence
            "plugins:\n  enabled: [other]\n",
            "plugins:\n  enabled: []\n",
            // plugins with nothing usable under it
            "plugins:\nmodel: sonnet\n",
            // comments and a following top-level key must survive
            "# top\nplugins:\n  # which ones\n  enabled:\n    - other\nmodel: sonnet\n",
        ] {
            let enabled = enable(original, PLUGIN);
            assert!(
                enabled.contains(PLUGIN),
                "enable did nothing for {original:?} -> {enabled:?}"
            );
            let disabled = disable(&enabled, PLUGIN);
            assert_eq!(
                normalize_for_compare(&disabled),
                normalize_for_compare(original),
                "disable(enable({original:?})) = {disabled:?} did not round-trip"
            );
        }
    }

    #[test]
    fn enabling_twice_changes_nothing_and_disabling_an_absent_plugin_changes_nothing() {
        let once = enable("plugins:\n  enabled:\n    - other\n", PLUGIN);
        assert_eq!(enable(&once, PLUGIN), once);
        for original in ["", "model: sonnet\n", "plugins:\n  enabled:\n    - other\n"] {
            assert_eq!(disable(original, PLUGIN), original);
        }
    }

    #[test]
    fn a_quoted_list_item_still_counts_as_present() {
        let quoted = format!("plugins:\n  enabled:\n    - \"{PLUGIN}\"\n");
        assert_eq!(
            enable(&quoted, PLUGIN),
            quoted,
            "requoted an existing entry"
        );
        assert_eq!(disable(&quoted, PLUGIN), "plugins:\n  enabled:\n");
    }

    /// An inline sequence taw cannot parse — an unterminated quote — is declined, not
    /// guessed at.
    #[test]
    fn an_unparseable_inline_sequence_is_left_alone() {
        let broken = "plugins: [\"unterminated\n";
        assert_eq!(disable(broken, PLUGIN), broken);
    }

    /// A file with no trailing newline keeps not having one.
    #[test]
    fn the_trailing_newline_is_preserved_either_way() {
        assert!(enable("plugins:\n  enabled:\n    - other", PLUGIN).ends_with("- other"));
        assert!(enable("plugins:\n  enabled:\n    - other\n", PLUGIN).ends_with('\n'));
    }

    // -------------------------------------------------------------------------------
    // Inline `enabled: [...]` — ported from herdr's `integration/tests.rs`
    // (`update_hermes_enabled_plugin`), herdr's plugin name replaced with taw's.
    // -------------------------------------------------------------------------------

    #[test]
    fn enable_converts_an_inline_enabled_list_to_a_block_list() {
        let config = enable("plugins:\n  enabled: [example-plugin]\n", PLUGIN);
        assert_eq!(
            config,
            format!("plugins:\n  enabled:\n    - {PLUGIN}\n    - example-plugin\n")
        );
    }

    #[test]
    fn enable_preserves_quoted_inline_enabled_items() {
        let config = enable("plugins:\n  enabled: [\"null\", 'foo: bar']\n", PLUGIN);
        assert_eq!(
            config,
            format!("plugins:\n  enabled:\n    - {PLUGIN}\n    - \"null\"\n    - 'foo: bar'\n")
        );
    }

    #[test]
    fn enable_preserves_an_inline_enabled_comment() {
        let config = enable(
            "plugins:\n  enabled: [example-plugin] # managed locally\n",
            PLUGIN,
        );
        assert_eq!(
            config,
            format!(
                "plugins:\n  enabled: # managed locally\n    - {PLUGIN}\n    - example-plugin\n"
            )
        );
    }

    #[test]
    fn enable_preserves_an_inline_plugins_comment() {
        let config = enable("plugins: [platforms/discord] # managed locally\n", PLUGIN);
        assert_eq!(
            config,
            format!("plugins: # managed locally\n  - {PLUGIN}\n  - platforms/discord\n")
        );
    }

    #[test]
    fn enable_is_idempotent_for_an_inline_enabled_list_entry() {
        let config = enable(
            &format!("plugins:\n  enabled: [{PLUGIN}, example-plugin]\n"),
            PLUGIN,
        );
        assert_eq!(
            config,
            format!("plugins:\n  enabled: [{PLUGIN}, example-plugin]\n")
        );
    }

    #[test]
    fn disable_removes_an_inline_enabled_list_entry() {
        let config = disable(
            &format!("plugins:\n  enabled: [example-plugin, {PLUGIN}]\n"),
            PLUGIN,
        );
        assert_eq!(config, "plugins:\n  enabled:\n    - example-plugin\n");
    }

    #[test]
    fn disable_preserves_quoted_inline_enabled_items() {
        let config = disable(
            &format!("plugins:\n  enabled: ['foo: bar', {PLUGIN}]\n"),
            PLUGIN,
        );
        assert_eq!(config, "plugins:\n  enabled:\n    - 'foo: bar'\n");
    }

    #[test]
    fn disable_converts_a_single_inline_enabled_entry_to_an_empty_list() {
        let config = disable(&format!("plugins:\n  enabled: [{PLUGIN}]\n"), PLUGIN);
        assert_eq!(config, "plugins:\n  enabled: []\n");
    }

    // -------------------------------------------------------------------------------
    // repair: the lines an older, unchecked edit left in a file Hermes cannot read
    // -------------------------------------------------------------------------------

    /// What the older edit wrote is exactly what `enable` still writes: running it over the
    /// shapes `check` refuses, then repairing, gives the user's file back.
    #[test]
    fn repair_takes_back_the_lines_enable_put_into_a_shape_it_does_not_know() {
        for theirs in [
            "plugins: null\nmodel: sonnet\n",
            "plugins: ~\n",
            "plugins: {a: 1}\nmodel: sonnet\n",
            "plugins: {enabled: [other]} # note\n",
            "plugins:\n  enabled: null\nmodel: sonnet\n",
            "plugins:\n  enabled: {a: 1}\n",
            "\"plugins\":\n  enabled:\n    - other\n",
            "model: sonnet\n'plugins':\n  enabled:\n    - other\nmore: 1\n",
        ] {
            let edited = enable(theirs, PLUGIN);
            assert_ne!(edited, theirs, "{theirs:?}");
            assert_eq!(
                repair(&edited, PLUGIN).as_deref(),
                Some(theirs),
                "{edited:?}"
            );
            // The file as the user wrote it holds none of taw's lines.
            assert_eq!(repair(theirs, PLUGIN), None, "{theirs:?}");
        }
    }

    #[test]
    fn repair_leaves_every_shape_the_edit_handles_alone() {
        for good in [
            "model: sonnet\n",
            "plugins:\n  enabled:\n    - taw-agent-state\n",
            "plugins:\n  - taw-agent-state\n",
            "plugins: [taw-agent-state]\n",
            "plugins:\n  enabled: [taw-agent-state]\n",
            // Broken, but the lines are not taw's: nothing of the user's is taken out.
            "plugins: null\n  enabled:\n    - other\n",
            "plugins:\n  enabled: null\n    - other\n",
            // Shapes the old edit broke that taw cannot tell from the user's own lines.
            "plugins:\n    enabled:\n      - other\n  enabled:\n    - taw-agent-state\n",
        ] {
            assert_eq!(repair(good, PLUGIN), None, "{good:?}");
        }
    }
}
