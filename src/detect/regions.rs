// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Region selection: the slice of a [`Snapshot`] a rule is matched against.
//!
//! Every helper returns a borrow of the snapshot, never an owned string, so
//! evaluating a manifest allocates nothing per rule.

use crate::detect::Snapshot;

/// Engine version that introduced `top_non_empty_lines(N)`.
const TOP_NON_EMPTY_LINES_ENGINE_VERSION: u32 = 3;
/// `top_non_empty_lines(N)` counts above this are rejected at parse time.
const MAX_TOP_REGION_LINE_COUNT: usize = u16::MAX as usize;

/// The text a rule with this `region` is matched against.
///
/// Returns `None` for a region name the engine does not know; [`parse_manifest`]
/// rejects those before a manifest is ever evaluated, so callers may treat `None`
/// as the empty region.
///
/// [`parse_manifest`]: crate::detect::parse_manifest
pub fn region_text<'a>(region: &str, snap: &Snapshot<'a>) -> Option<&'a str> {
    let spec = region.trim();

    // OSC regions source from their dedicated fields, not the screen.
    match spec {
        "osc_title" => return Some(snap.osc_title),
        "osc_progress" => return Some(snap.osc_progress),
        _ => {}
    }

    let content = snap.content;
    let text = match spec {
        "whole_recent" => content,
        "after_last_prompt_marker" => after_last_prompt_marker(content),
        "before_current_prompt_marker" => before_current_prompt_marker(content),
        "whole_recent_without_current_prompt_marker" => {
            whole_recent_without_current_prompt_marker(content)
        }
        "current_prompt_block_marker" => current_prompt_block_marker(content).unwrap_or(""),
        "after_current_prompt_block_marker" => {
            after_current_prompt_block_marker(content).unwrap_or("")
        }
        "prompt_box_body" => prompt_box_body(content).unwrap_or(""),
        "above_prompt_box" => above_prompt_box(content),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(content),
        _ => {
            if let Some(count) = region_count(spec, "bottom_lines") {
                bottom_lines(content, count)
            } else if let Some(count) = region_count(spec, "bottom_non_empty_lines") {
                bottom_non_empty_lines(content, count)
            } else if let Some(count) = top_region_count(spec) {
                top_non_empty_lines(content, count)
            } else {
                return None;
            }
        }
    };
    Some(text)
}

/// `Ok(())` when the engine knows this region name, `Err(name)` otherwise.
pub(crate) fn validate_region_name(spec: &str) -> Result<(), String> {
    let trimmed = spec.trim();
    match trimmed {
        "whole_recent"
        | "after_last_prompt_marker"
        | "before_current_prompt_marker"
        | "whole_recent_without_current_prompt_marker"
        | "current_prompt_block_marker"
        | "after_current_prompt_block_marker"
        | "prompt_box_body"
        | "above_prompt_box"
        | "last_non_empty_above_prompt_box"
        | "after_last_horizontal_rule"
        | "osc_title"
        | "osc_progress" => Ok(()),
        _ if region_count(trimmed, "bottom_lines").is_some()
            || region_count(trimmed, "bottom_non_empty_lines").is_some()
            || top_region_count(trimmed).is_some() =>
        {
            Ok(())
        }
        _ => Err(trimmed.to_string()),
    }
}

/// The engine version a region requires, when it is newer than version 1.
pub(crate) fn min_engine_version_for_region(spec: &str) -> Option<u32> {
    spec.trim()
        .starts_with("top_non_empty_lines(")
        .then_some(TOP_NON_EMPTY_LINES_ENGINE_VERSION)
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    spec.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|count| count.parse::<usize>().ok())
}

fn top_region_count(spec: &str) -> Option<usize> {
    let count = spec
        .strip_prefix("top_non_empty_lines")?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    if count.starts_with('0') || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    count
        .parse::<usize>()
        .ok()
        .filter(|count| *count <= MAX_TOP_REGION_LINE_COUNT)
}

fn bottom_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(count);
    slice_from_line_index(content, &lines, start)
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start_index) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line_index(content, &lines, start_index)
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(end_index) = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    let byte_offset = line_start_offset(content, &lines, end_index + 1);
    &content[..byte_offset]
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines.iter().rposition(|line| codex_prompt_line(line)) else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
}

fn before_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = current_codex_prompt_index(&lines) else {
        return content;
    };
    let byte_offset = lines[..index]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>();
    &content[..byte_offset.min(content.len())]
}

fn whole_recent_without_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    if current_codex_prompt_index(&lines).is_some() {
        ""
    } else {
        content
    }
}

fn current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    lines[..prompt_index]
        .iter()
        .rev()
        .find(|line| codex_block_marker_line(line))
        .copied()
}

fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    let block_index = lines[..prompt_index]
        .iter()
        .rposition(|line| codex_block_marker_line(line))?;
    Some(slice_from_line_index(content, &lines, block_index))
}

fn current_codex_prompt_index(lines: &[&str]) -> Option<usize> {
    let prompt_index = lines.iter().rposition(|line| codex_prompt_line(line))?;
    if lines[prompt_index + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line))
    {
        return None;
    }
    Some(prompt_index)
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start.min(content.len())..end.min(content.len())])
}

fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top) = prompt_box_top_border_index(&lines) else {
        return content;
    };
    let end = line_start_offset(content, &lines, top);
    &content[..end.min(content.len())]
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            border_count += 1;
            if border_count == 2 {
                return Some(index);
            }
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }

    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();

    suffix.is_empty() || rule_chars >= 3
}

fn slice_from_line_index<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    let byte_offset = line_start_offset(content, lines, index);
    &content[byte_offset.min(content.len())..]
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

// Follows herdr's src/detect/manifest/tests.rs. Differences:
// adapted to taw types.
#[cfg(test)]
mod tests {
    use super::*;

    fn screen(content: &str) -> Snapshot<'_> {
        Snapshot {
            content,
            osc_title: "",
            osc_progress: "",
        }
    }

    fn region<'a>(snap: &Snapshot<'a>, spec: &str) -> &'a str {
        region_text(spec, snap).expect("known region")
    }

    #[test]
    fn bottom_non_empty_lines_uses_bottom_occurrence_for_repeated_text() {
        let content = "marker\nold\n\nmiddle\nmarker\nnew\n";

        assert_eq!(
            region(&screen(content), "bottom_non_empty_lines(2)"),
            "marker\nnew\n"
        );
    }

    #[test]
    fn top_non_empty_lines_uses_top_occurrence_for_repeated_text() {
        let content = "\nmarker\nold\n\nmiddle\nmarker\nnew\n";

        assert_eq!(
            region(&screen(content), "top_non_empty_lines(2)"),
            "\nmarker\nold\n"
        );
    }

    #[test]
    fn top_non_empty_lines_requires_a_canonical_positive_bounded_count() {
        let name = "top_non_empty_lines";
        assert!(validate_region_name(&format!("{name}(1)")).is_ok());
        assert!(validate_region_name(&format!("{name}({})", u16::MAX)).is_ok());
        for count in ["0", "01", "+1", "65536", "999999999999999999999999"] {
            assert!(
                validate_region_name(&format!("{name}({count})")).is_err(),
                "{name} accepted invalid count {count}"
            );
        }
    }

    #[test]
    fn osc_regions_read_their_dedicated_fields_not_the_screen() {
        let snap = Snapshot {
            content: "screen text",
            osc_title: "✳ project",
            osc_progress: "4;1;40",
        };

        assert_eq!(region(&snap, "osc_title"), "✳ project");
        assert_eq!(region(&snap, " osc_progress "), "4;1;40");
        assert_eq!(region(&snap, "whole_recent"), "screen text");
    }

    #[test]
    fn unknown_region_names_have_no_text() {
        assert!(region_text("after_last_promt_marker", &screen("x")).is_none());
        assert!(region_text("bottom_non_empty_lines(x)", &screen("x")).is_none());
    }

    #[test]
    fn prompt_box_body_is_the_text_between_the_last_two_rules() {
        let content = "above\n─────\n❯ typed\n─────\n? for shortcuts\n";

        assert_eq!(region(&screen(content), "prompt_box_body"), "❯ typed\n");
        assert_eq!(region(&screen(content), "above_prompt_box"), "above\n");
        assert_eq!(
            region(&screen(content), "last_non_empty_above_prompt_box"),
            "above"
        );
        assert_eq!(
            region(&screen(content), "after_last_horizontal_rule"),
            "? for shortcuts\n"
        );
    }

    #[test]
    fn short_dashes_are_only_a_rule_when_nothing_follows_them() {
        // herdr's `is_horizontal_rule`: a suffix is allowed only after 3+ dashes.
        let content = "a\n── label\n───────── label\nb\n";
        // Only the 9-dash line counts, so there is one rule and no prompt box.
        assert_eq!(
            region(&screen(content), "after_last_horizontal_rule"),
            "b\n"
        );
        assert_eq!(region(&screen(content), "prompt_box_body"), "");
    }

    #[test]
    fn codex_prompt_regions_track_the_live_prompt_line() {
        let content = "• ran a tool\n› typed text\n";

        assert_eq!(region(&screen(content), "after_last_prompt_marker"), "");
        assert_eq!(
            region(&screen(content), "before_current_prompt_marker"),
            "• ran a tool\n"
        );
        assert_eq!(
            region(&screen(content), "current_prompt_block_marker"),
            "• ran a tool"
        );
        assert_eq!(
            region(
                &screen(content),
                "whole_recent_without_current_prompt_marker"
            ),
            ""
        );

        // A block marker after the prompt means the prompt is scrollback, not live.
        let transcript = "› typed text\n• ran a tool\n";
        assert_eq!(
            region(
                &screen(transcript),
                "whole_recent_without_current_prompt_marker"
            ),
            transcript
        );
    }
}
