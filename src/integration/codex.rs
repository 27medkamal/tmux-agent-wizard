// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Codex CLI integration.
//!
//! Writes `<CODEX_HOME|~/.codex>/taw-agent-state.sh`, adds a `hooks.SessionStart[*]` entry
//! to `<same dir>/hooks.json`, and makes `<same dir>/config.toml` say `[features] hooks = true`
//! (codex only reads hooks when it does). Codex keeps a `[features]` table of its own in that
//! file and refuses to start on a second one, so the line goes into the user's table when
//! there is one, and a marked `[features]` table goes at the end only when there is none. Only
//! marked lines are written, never a delimited block: Codex saves its own switches into the table
//! it finds, and a block taw strips whole would take them with it. An older taw's block is turned
//! into the user's own lines. The result is parsed before it is written; a file that would not
//! parse is left alone.
//!
//! Follows herdr's `codex` integration — see
//! `third_party/herdr/integrations/codex/herdr-agent-state.sh`, `hooks.json` and
//! `config.toml`.

use super::{
    HOOKS_KEY, Incomplete, IntegrationError, Report, Status, TemplateLang, blanked, env_dir,
    has_toml_block, home_dir, parse_version_marker, plan_json_hook, plan_text_edit_repairing,
    remove_toml_block_at, render_template, require_config_dir, strip_toml_block, toml_block_ranges,
    toml_problem, uninstall_json_hook, uninstall_text_edit, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-codex.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const EVENT: &str = "SessionStart";
const AGENT: &str = "codex";
/// codex only reads `hooks.json` when this feature flag is on. This is the line that goes into
/// the user's own `[features]` table, and under a marked table at the end of a `config.toml` that
/// has no `features` at all. Marked, so an uninstall takes out this line and nothing else of
/// theirs.
const FEATURES_LINE: &str = "hooks = true  # taw integration";

fn config_dir() -> Result<PathBuf> {
    match env_dir("CODEX_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".codex")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("taw-agent-state.sh"))
}

fn hooks_json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks.json"))
}

fn config_toml_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

fn hooks_sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.json");
    PathBuf::from(name)
}

fn toml_sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.toml");
    PathBuf::from(name)
}

fn hook_entry(script: &Path) -> Value {
    json!({
        "hooks": [
            {
                "command": format!("bash '{}' session", script.display()),
                "timeout": 10,
                "type": "command"
            }
        ]
    })
}

/// How `features` is written when taw cannot put a line into it, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShapeKind {
    /// `[features]`, the user's own table
    Table,
    /// `features.goals = true`
    Dotted,
    /// `features = { goals = true }`
    Inline,
    /// `["features"]`
    QuotedHeader,
    /// `[features.<name>]` and no `[features]` of its own
    SubTable,
    /// `[[features]]`
    ArrayOfTables,
    /// `features = 3`, `features = [1]`: not a table at all
    NotATable,
}

#[derive(Debug, Clone, Copy)]
struct Shape {
    kind: ShapeKind,
    line: usize,
}

/// Why taw does not edit a `config.toml`.
enum Refusal {
    /// The file does not parse as TOML, with taw's own older block left out of the reading.
    NotToml { message: String, line: usize },
    /// `[features]` sets `hooks = false`: the user switched it off, and taw does not switch it
    /// back. `line` is where, when it can be found.
    HooksOff { line: Option<usize> },
    /// `hooks` under `[features]` is not a boolean.
    HooksNotABool { found: &'static str },
    /// `features` is written as dotted keys, an inline table, a quoted header, a sub-table or a
    /// table of tables, none of which taw adds a line to.
    ByHand(Option<Shape>),
    /// taw's edit did not come out as a file that parses and holds `features.hooks = true` and
    /// nothing else changed: it is not written.
    Unchecked,
    /// The keys an older taw block holds and the user's own `[features]` table both set `key`.
    Conflict {
        key: String,
        block_line: Option<usize>,
        their_line: Option<usize>,
    },
    /// `features` is written in a way taw adds no line to, and an older taw's block holds keys of
    /// the user's that would have to move into it. `lines` are the block's lines that hold them;
    /// `loads` is whether Codex could load the file as it is (two definitions of `features` stop
    /// it).
    BlockKeys {
        shape: Option<Shape>,
        lines: Vec<usize>,
        loads: bool,
    },
}

/// "line 8", "lines 8 and 9", "lines 8, 9 and 12".
fn lines_phrase(lines: &[usize]) -> String {
    match lines {
        [] => "the block".to_string(),
        [one] => format!("line {one}"),
        [init @ .., last] => {
            let init: Vec<String> = init.iter().map(usize::to_string).collect();
            format!("lines {} and {last}", init.join(", "))
        }
    }
}

/// How `features` is written, and where `hooks = true` goes in it.
fn describe(shape: &Option<Shape>) -> (String, &'static str) {
    let at = |line: usize, what: &str| format!("line {line} {what}");
    match shape {
        Some(Shape {
            kind: ShapeKind::Table,
            line,
        }) => (
            at(*line, "is your [features] table"),
            "add hooks = true under that header",
        ),
        Some(Shape {
            kind: ShapeKind::Dotted,
            line,
        }) => (
            at(
                *line,
                "writes features as dotted keys (features.<name> = ...)",
            ),
            "add features.hooks = true next to them",
        ),
        Some(Shape {
            kind: ShapeKind::Inline,
            line,
        }) => (
            at(*line, "writes features as an inline table ({ ... })"),
            "add hooks = true inside its braces",
        ),
        Some(Shape {
            kind: ShapeKind::QuotedHeader,
            line,
        }) => (
            at(*line, "is a quoted [\"features\"] header"),
            "add hooks = true under that header",
        ),
        Some(Shape {
            kind: ShapeKind::SubTable,
            line,
        }) => (
            at(
                *line,
                "is a [features.<name>] sub-table, and the file has no [features] table of its own",
            ),
            "add a [features] table with hooks = true",
        ),
        Some(Shape {
            kind: ShapeKind::ArrayOfTables,
            line,
        }) => (
            at(*line, "is an array of tables ([[features]])"),
            "change it to a [features] table with hooks = true",
        ),
        Some(Shape {
            kind: ShapeKind::NotATable,
            line,
        }) => (
            at(*line, "sets features to something that is not a table"),
            "change it to a [features] table with hooks = true",
        ),
        None => (
            "features is written in a way taw does not edit".to_string(),
            "add hooks = true to your [features] table",
        ),
    }
}

impl Refusal {
    /// What the install says, after "<path> is not a config this can edit: ". `again` is the
    /// command the user runs once they have changed the file ("install" or "uninstall").
    fn detail(&self, again: &str) -> String {
        const WHY: &str = "taw's hook runs only with hooks = true";
        match self {
            Refusal::NotToml { message, line } => format!(
                "it does not parse as TOML ({message}, line {line}), so Codex cannot load it \
                 either; taw left it as it is. Fix the file, then {again} again"
            ),
            Refusal::HooksOff { line } => {
                let place = match line {
                    Some(line) => format!("line {line} sets"),
                    None => "[features] sets".to_string(),
                };
                format!(
                    "{place} hooks = false, and {WHY}. You switched it off, and taw does not \
                     switch it back: change it to true yourself, then {again} again"
                )
            }
            Refusal::HooksNotABool { found } => format!(
                "hooks under [features] is a {found}, and {WHY}. Change it to true yourself, \
                 then {again} again"
            ),
            Refusal::ByHand(shape) => {
                let (found, add) = describe(shape);
                format!(
                    "{found}, which taw does not edit, and {WHY}. Please {add} by hand, then \
                     {again} again"
                )
            }
            Refusal::BlockKeys {
                shape,
                lines,
                loads,
            } => {
                let (found, _) = describe(shape);
                let block = lines_phrase(lines);
                let stop = if *loads {
                    "taw cannot move them into features the way it is written"
                } else {
                    "Codex cannot load the file while both define features"
                };
                format!(
                    "{found}, and the older taw block ({block}) holds keys of yours as well: \
                     {stop}. Please move the block's keys into your own features, written the \
                     way yours is, and delete the block, then {again} again"
                )
            }
            Refusal::Unchecked => format!(
                "taw could not add hooks = true to it in a way that leaves the rest of the file \
                 as it is, and {WHY}. Add hooks = true under [features] by hand, then {again} \
                 again"
            ),
            Refusal::Conflict {
                key,
                block_line,
                their_line,
            } => {
                let lines = match (block_line, their_line) {
                    (Some(block), Some(theirs)) => {
                        format!("line {block} in the older taw block and line {theirs} outside it")
                    }
                    (Some(line), None) | (None, Some(line)) => format!("line {line}"),
                    (None, None) => "two places".to_string(),
                };
                format!(
                    "the older taw block holds a key of yours, {key}, that your own [features] \
                     table sets as well ({lines}), and taw cannot tell which of the two you \
                     want. Delete one of them, then {again} again"
                )
            }
        }
    }
}

/// A refusal, and the file as the install leaves it instead when taw's older block had to come
/// out of it.
struct Refused {
    refusal: Refusal,
    repaired: Option<String>,
    /// The file did not parse as it was: the older block was what stopped Codex from loading it.
    broke: bool,
}

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Refused {
            refusal,
            repaired: None,
            broke: false,
        }
    }
}

impl Refused {
    /// The install's error, and (when the older block came out) the file to leave.
    fn into_parts(self, path: &Path) -> (anyhow::Error, Option<String>) {
        let lead = match (self.repaired.is_some(), self.broke) {
            (true, true) => {
                "taw took its older block out of it, keeping every key of yours, since that \
                 block stopped Codex from loading the file, and changed nothing else. taw does \
                 not install yet: "
            }
            (true, false) => {
                "taw took its older block out of it, keeping every key of yours, and changed \
                 nothing else. taw does not install yet: "
            }
            (false, _) => "",
        };
        let error = IntegrationError::UnusableConfig {
            path: path.display().to_string(),
            detail: format!("{lead}{}", self.refusal.detail("install")),
        }
        .into();
        (error, self.repaired)
    }
}

fn not_toml(text: &str) -> Refusal {
    let (message, line) = toml_problem(text);
    Refusal::NotToml { message, line }
}

/// Whether `line` is the header `[features]`, with spaces around the name or a comment after
/// it allowed.
fn is_features_header(line: &str) -> bool {
    let line = line.trim();
    let Some(inner) = line.strip_prefix('[') else {
        return false;
    };
    let Some((name, rest)) = inner.split_once(']') else {
        return false;
    };
    let rest = rest.trim_start();
    name.trim() == "features" && (rest.is_empty() || rest.starts_with('#'))
}

/// The header taw writes when the file has no `[features]` table, and the one an older block's
/// header becomes: marked, so an uninstall can tell it from the user's own.
const MARKED_HEADER: &str = "[features]  # taw integration";

/// `[features]  # taw integration`, however it is spaced.
fn is_marked_header(line: &str) -> bool {
    line.split_whitespace()
        .eq(["[features]", "#", "taw", "integration"])
}

/// The line, counted from 1, that holds byte `offset` of `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// The spans and values of the file's `features` entry: the key's byte range, and the value.
fn features_entry(text: &str) -> Option<(std::ops::Range<usize>, toml::de::DeValue<'_>)> {
    let document = toml::de::DeTable::parse(text).ok()?;
    let (key, value) = document
        .get_ref()
        .iter()
        .find(|(key, _)| key.get_ref().as_ref() == "features")?;
    Some((key.span(), value.get_ref().clone()))
}

/// How `features` is written in `text`, and on which line, read from the parser's own spans (so
/// a header-shaped line inside a multi-line string is never taken for it). The lines of taw's
/// older blocks are left blank but counted.
fn features_shape(text: &str) -> Option<Shape> {
    let blank = blanked(text);
    let (span, value) = features_entry(&blank)?;
    let line = line_of(&blank, span.start);
    let line_start = blank[..span.start].rfind('\n').map_or(0, |at| at + 1);
    let before = blank[line_start..span.start].trim();
    let after = blank[span.end..].trim_start();
    let kind = if before.ends_with("[[") {
        ShapeKind::ArrayOfTables
    } else if before.ends_with('[') {
        if blank[span.start..].starts_with(['"', '\'']) {
            ShapeKind::QuotedHeader
        } else if after.starts_with(']') {
            ShapeKind::Table
        } else {
            ShapeKind::SubTable
        }
    } else if after.starts_with('.') {
        ShapeKind::Dotted
    } else if matches!(value, toml::de::DeValue::Table(_)) {
        ShapeKind::Inline
    } else {
        ShapeKind::NotATable
    };
    Some(Shape { kind, line })
}

/// The line, counted from 1, where `key` is set under `features` in `text` (taw's older blocks
/// blanked out), when it can be found.
fn features_key_line(text: &str, key: &str) -> Option<usize> {
    let blank = blanked(text);
    let (_, value) = features_entry(&blank)?;
    let toml::de::DeValue::Table(table) = value else {
        return None;
    };
    let (key, _) = table.iter().find(|(k, _)| k.get_ref().as_ref() == key)?;
    Some(line_of(&blank, key.span().start))
}

/// `text` with `new_lines` right under a `[features]` header, in the file's own line ending,
/// where the result parses to `expected`. Each header-shaped line is tried in order and the first
/// that gives `expected` is used, so a line that only looks like the header inside a multi-line
/// string fails the test and the real table further down is found. `None` when none does.
fn insert_under_header(text: &str, new_lines: &[&str], expected: &toml::Table) -> Option<String> {
    let wanted = format!("{expected:?}");
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        if is_features_header(line.trim_end_matches(['\n', '\r'])) {
            let eol = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
            let mut out = text[..end].to_string();
            if !line.ends_with('\n') {
                out.push_str(eol);
            }
            for new in new_lines {
                out.push_str(new);
                out.push_str(eol);
            }
            out.push_str(&text[end..]);
            // Compared as text: a `nan` is not equal to itself, but prints the same.
            if out
                .parse::<toml::Table>()
                .is_ok_and(|reparsed| format!("{reparsed:?}") == wanted)
            {
                return Some(out);
            }
        }
        offset = end;
    }
    None
}

/// `hooks = true  # taw integration` and `hooks = true` alike, however spaced.
fn is_hooks_true(line: &str) -> bool {
    let line = line.split('#').next().unwrap_or("");
    line.split_whitespace().eq(["hooks", "=", "true"])
}

/// An older taw's delimited block, read: where it is, and the lines in it that are not taw's.
struct OldBlock {
    range: std::ops::Range<usize>,
    /// The lines of the block that are neither its markers, its `[features]` header nor taw's
    /// `hooks = true`: keys Codex saved into the table the block gave it. With their line
    /// numbers in the file.
    kept: Vec<(usize, String)>,
    /// taw's own `hooks = true` was among its lines (and is not in `kept`).
    removed_hooks: bool,
    /// Blank lines between the block's last taw line and the first line in `kept`.
    leading_blanks: usize,
    /// The key lines of the block's own `[features]` table: the lines between its header and its
    /// first table, comments and blank lines left out, with their line numbers in the file.
    keys: Vec<(usize, String)>,
    /// Every other line of the block (not its markers, its header, taw's `hooks = true` or the
    /// key lines): comments, blank lines, and the tables Codex saved into it. They stay where
    /// they are.
    stay: Vec<String>,
}

/// Whether `line` is neither blank nor a comment.
fn is_key_line(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && !line.starts_with('#')
}

fn old_blocks(text: &str) -> Vec<OldBlock> {
    toml_block_ranges(text)
        .into_iter()
        .map(|range| {
            let first = line_of(text, range.start);
            let mut kept = Vec::new();
            let (mut keys, mut stay) = (Vec::new(), Vec::new());
            let (mut header, mut hooks, mut in_table) = (false, false, false);
            let lines: Vec<&str> = text[range.clone()].lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if index == 0 || index + 1 == lines.len() {
                    continue; // the two markers
                }
                if !header && is_features_header(line) {
                    header = true;
                    in_table = true;
                } else if in_table && !hooks && is_hooks_true(line) {
                    // Taw's own line is the one in the block's table, not one a table Codex
                    // saved after it happens to hold.
                    hooks = true;
                } else {
                    if line.trim_start().starts_with('[') {
                        in_table = false;
                    }
                    if in_table && is_key_line(line) {
                        keys.push((first + index, line.to_string()));
                    } else {
                        stay.push(line.to_string());
                    }
                    kept.push((first + index, line.to_string()));
                }
            }
            while stay.last().is_some_and(|line| line.trim().is_empty()) {
                stay.pop();
            }
            while kept.last().is_some_and(|(_, line)| line.trim().is_empty()) {
                kept.pop();
            }
            let mut leading_blanks = 0;
            while kept.first().is_some_and(|(_, line)| line.trim().is_empty()) {
                kept.remove(0);
                leading_blanks += 1;
            }
            OldBlock {
                range,
                kept,
                removed_hooks: hooks,
                leading_blanks,
                keys,
                stay,
            }
        })
        .collect()
}

/// `text` with every older taw block turned into what it was for the user: its two markers and
/// its `hooks = true` go, and its other lines stay. With a `[features]` table of the user's in the
/// file, those lines move under its header; with none, the block's own header stays as the table
/// (marked when `mark` is set, so an uninstall can remove it once it is empty). A key set in
/// both is refused. A file without a block comes back as it is.
fn convert(text: &str, mark: bool) -> Result<String, Refused> {
    convert_blocks(text, mark).map(|out| keep_final_newline(text, out))
}

/// `out` ends in a newline when `original` did: a repair that cuts a block off the end of a file
/// does not take the file's last line break with it.
fn keep_final_newline(original: &str, mut out: String) -> String {
    if original.ends_with('\n') && !out.is_empty() && !out.ends_with('\n') {
        out.push_str(if original.ends_with("\r\n") {
            "\r\n"
        } else {
            "\n"
        });
    }
    out
}

/// `lines` as they replace a block that starts at the end of `before`: joined by newlines, with
/// the blank lines at their start dropped when `before` already ends in a blank line (the
/// separator an older install put there) and cut to one otherwise.
fn set_off(before: &str, lines: &[String]) -> String {
    let lead = lines
        .iter()
        .take_while(|line| line.trim().is_empty())
        .count();
    let keep = if before.is_empty() || before.ends_with("\n\n") {
        0
    } else {
        lead.min(1)
    };
    lines[lead - keep..].join("\n")
}

/// `text` with `block` replaced by the lines of it that stay where they are; the block alone
/// goes, with its separator, when none do.
fn replace_with_stay(text: &str, block: &OldBlock) -> String {
    if block.stay.is_empty() {
        return remove_toml_block_at(text, block.range.clone());
    }
    let mut out = text.to_string();
    let replacement = set_off(&text[..block.range.start], &block.stay);
    out.replace_range(block.range.clone(), &replacement);
    out
}

/// `from` merged into `into`, tables into tables; two values for one key refuse.
fn merge_tables(into: &mut toml::Table, from: toml::Table) -> Result<(), Refusal> {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (None, value) => {
                into.insert(key, value);
            }
            (Some(toml::Value::Table(a)), toml::Value::Table(b)) => merge_tables(a, b)?,
            _ => return Err(Refusal::Unchecked),
        }
    }
    Ok(())
}

fn convert_blocks(text: &str, mark: bool) -> Result<String, Refused> {
    let blocks = old_blocks(text);
    if blocks.is_empty() {
        return Ok(text.to_string());
    }
    let base = strip_toml_block(text);
    // The file does not parse without the blocks: say so only when it does not parse as it
    // stands either. A file that loads is no case of "not toml"; the repair is what failed.
    let parsed: toml::Table = base.parse().map_err(|_| {
        if text.parse::<toml::Table>().is_ok() {
            Refusal::Unchecked
        } else {
            not_toml(text)
        }
    })?;
    let kept: Vec<&(usize, String)> = blocks.iter().flat_map(|block| &block.kept).collect();
    if kept.is_empty() {
        return Ok(base);
    }
    let has_table_line = kept
        .iter()
        .any(|(_, line)| line.trim_start().starts_with('['));
    let parsed_keys = || -> Result<toml::Table, Refusal> {
        let kept_text: String = kept.iter().map(|(_, line)| format!("{line}\n")).collect();
        kept_text.parse().map_err(|_| Refusal::Unchecked)
    };
    // The lines of the blocks that hold keys (not blanks, comments or tables), for a message.
    let key_lines = || -> Vec<usize> {
        blocks
            .iter()
            .flat_map(|block| &block.keys)
            .map(|(line, _)| *line)
            .collect()
    };
    let block_keys = || Refusal::BlockKeys {
        shape: features_shape(text),
        lines: key_lines(),
        loads: text.parse::<toml::Table>().is_ok(),
    };
    let mut expected = parsed.clone();
    match parsed.get("features") {
        None => {
            // One block and a file that loads as it is: nothing moves. The two markers and taw's
            // own line go, the header gets the mark, and every other line, the tables Codex
            // saved into the block included, stays where it is. The result must parse to the
            // original (minus taw's line). On an uninstall the header is taw's alone, and goes
            // too when no key of theirs is left under it.
            let drop_header = !mark && blocks.iter().all(|block| block.keys.is_empty());
            let single = match (blocks.as_slice(), text.parse::<toml::Table>()) {
                ([block], Ok(original)) => Some((block.removed_hooks, original)),
                _ => None,
            };
            let single_block = single.is_some();
            let expected = match single {
                Some((removed_hooks, mut original)) => {
                    if removed_hooks
                        && let Some(toml::Value::Table(features)) = original.get_mut("features")
                    {
                        features.remove("hooks");
                    }
                    if drop_header
                        && original
                            .get("features")
                            .and_then(toml::Value::as_table)
                            .is_some_and(toml::Table::is_empty)
                    {
                        original.remove("features");
                    }
                    original
                }
                None => {
                    // Several blocks: their keys are put together under one header, which only
                    // works for keys alone, with no table of their own.
                    if has_table_line {
                        return Err(Refusal::Unchecked.into());
                    }
                    if !drop_header {
                        expected.insert("features".to_string(), parsed_keys()?.into());
                    }
                    expected
                }
            };
            let mut replacement;
            if drop_header {
                // Nothing of theirs is left under the header, so the header was taw's alone and
                // goes; the comments and tables under it stay where they are.
                let lines: Vec<String> = kept.iter().map(|(_, line)| line.clone()).collect();
                replacement = set_off(&text[..blocks[0].range.start], &lines);
            } else {
                let header = if mark { MARKED_HEADER } else { "[features]" };
                replacement = header.to_string();
                if single_block {
                    // Blank lines that set a table off from taw's line stay too.
                    replacement.push_str(&"\n".repeat(blocks[0].leading_blanks));
                }
                for (_, line) in &kept {
                    replacement.push('\n');
                    replacement.push_str(line);
                }
            }
            let mut out = text.to_string();
            for (index, block) in blocks.iter().enumerate().rev() {
                if index == 0 {
                    out.replace_range(block.range.clone(), &replacement);
                } else {
                    out = remove_toml_block_at(&out, block.range.clone());
                }
            }
            check_same(&out, &expected)?;
            Ok(out)
        }
        Some(value) => {
            // The block's keys move under their table's header; its comments, blank lines and
            // tables stay where they are, as Codex left them.
            let key_text: String = blocks
                .iter()
                .flat_map(|block| &block.keys)
                .map(|(_, line)| format!("{line}\n"))
                .collect();
            let keys: toml::Table = key_text.parse().map_err(|_| Refusal::Unchecked)?;
            let theirs = value.as_table();
            if let Some(key) = keys
                .keys()
                .find(|key| theirs.is_some_and(|theirs| theirs.contains_key(*key)))
            {
                let block_line = blocks
                    .iter()
                    .flat_map(|block| &block.keys)
                    .find(|(_, line)| sets_key(line, key))
                    .map(|(line, _)| *line);
                return Err(Refusal::Conflict {
                    key: key.clone(),
                    block_line,
                    their_line: features_key_line(text, key),
                }
                .into());
            }
            let moves_keys = !keys.is_empty();
            if let Some(toml::Value::Table(features)) = expected.get_mut("features") {
                features.extend(keys);
            }
            let stay_text: String = blocks
                .iter()
                .flat_map(|block| &block.stay)
                .map(|line| format!("{line}\n"))
                .collect();
            let stay: toml::Table = stay_text.parse().map_err(|_| Refusal::Unchecked)?;
            merge_tables(&mut expected, stay)?;
            let mut out = text.to_string();
            for block in blocks.iter().rev() {
                out = replace_with_stay(&out, block);
            }
            if !moves_keys {
                check_same(&out, &expected)?;
                return Ok(out);
            }
            let lines: Vec<&str> = blocks
                .iter()
                .flat_map(|block| &block.keys)
                .map(|(_, line)| line.as_str())
                .collect();
            insert_under_header(&out, &lines, &expected).ok_or_else(|| block_keys().into())
        }
    }
}

/// `result` parses, and to `expected`.
fn check_same(result: &str, expected: &toml::Table) -> Result<(), Refusal> {
    let reparsed: toml::Table = result.parse().map_err(|_| Refusal::Unchecked)?;
    // Compared as text: a `nan` is not equal to itself, but prints the same.
    if format!("{reparsed:?}") == format!("{expected:?}") {
        Ok(())
    } else {
        Err(Refusal::Unchecked)
    }
}

/// Whether `line` sets `key`, quoted or not.
fn sets_key(line: &str, key: &str) -> bool {
    let line = line.trim_start();
    let line = line.strip_prefix(['"', '\'']).unwrap_or(line);
    let Some(rest) = line.strip_prefix(key) else {
        return false;
    };
    let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
    rest.trim_start().starts_with('=')
}

/// What `config.toml` becomes: taw's older block turned into marked lines, then `hooks = true`
/// into the user's own `[features]` table, or a marked table at the end when the file has no
/// `features`. The result is parsed and compared with the parsed original before it is returned,
/// so what is written is a file Codex can load, holding `features.hooks = true`, and the same
/// otherwise. A refusal after the older block came out carries the file without it.
fn merged(text: &str) -> Result<String, Refused> {
    let converted = convert(text, true)?;
    add_hooks(&converted).map_err(|refusal| Refused {
        refusal,
        repaired: (converted != text).then_some(converted.clone()),
        broke: text.parse::<toml::Table>().is_err(),
    })
}

/// `text` (without any older block) with `hooks = true` in its `[features]` table.
fn add_hooks(text: &str) -> Result<String, Refusal> {
    let parsed: toml::Table = text.parse().map_err(|_| not_toml(text))?;
    let mut expected = parsed.clone();
    let features = expected
        .entry("features")
        .or_insert_with(|| toml::Table::new().into());
    if let Some(features) = features.as_table_mut() {
        features.insert("hooks".to_string(), true.into());
    }
    let result = match parsed.get("features") {
        None => {
            let mut out = text.to_string();
            // One blank line sets the table off.
            if !out.is_empty() {
                out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
            }
            out.push_str(&format!("{MARKED_HEADER}\n{FEATURES_LINE}\n"));
            out
        }
        Some(toml::Value::Table(features)) => match features.get("hooks") {
            Some(toml::Value::Boolean(true)) => return Ok(text.to_string()),
            Some(toml::Value::Boolean(false)) => {
                return Err(Refusal::HooksOff {
                    line: features_key_line(text, "hooks"),
                });
            }
            Some(other) => {
                return Err(Refusal::HooksNotABool {
                    found: other.type_str(),
                });
            }
            None => insert_under_header(text, &[FEATURES_LINE], &expected).ok_or_else(|| {
                match features_shape(text) {
                    Some(shape) => Refusal::ByHand(Some(shape)),
                    None => Refusal::Unchecked,
                }
            })?,
        },
        Some(_) => return Err(Refusal::ByHand(features_shape(text))),
    };
    check_same(&result, &expected)?;
    Ok(result)
}

/// Whether `line` is the one [`add_hooks`] writes, however it is spaced.
fn is_taws_line(line: &str) -> bool {
    line.split_whitespace()
        .eq(["hooks", "=", "true", "#", "taw", "integration"])
}

/// `text` without what an install of taw's put in it: the marked `hooks` line, then a marked
/// `[features]` header when nothing but blank lines is left in its table. An older taw's block is
/// converted first. A header that is not marked is the user's and stays.
fn strip_taw(text: &str) -> String {
    keep_final_newline(text, strip_taw_lines(text))
}

fn strip_taw_lines(text: &str) -> String {
    let converted = match convert(text, false) {
        Ok(converted) => converted,
        Err(Refused {
            refusal: Refusal::NotToml { .. },
            ..
        }) => strip_toml_block(text),
        // Not convertible (the install refuses such a file too): at least the markers and taw's
        // own line go.
        Err(_) => strip_markers_only(text),
    };
    let lines: Vec<&str> = converted
        .split_inclusive('\n')
        .filter(|line| !is_taws_line(line))
        .collect();
    let Some(header) = lines.iter().position(|line| is_marked_header(line)) else {
        return lines.concat();
    };
    let table_end = lines[header + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| header + 1 + offset);
    if !lines[header + 1..table_end]
        .iter()
        .all(|line| line.trim().is_empty())
    {
        return lines.concat();
    }
    // The header goes, and the blank lines before it that set the table off (an install puts it
    // two lines after the last thing in the file); the blank lines after it stay.
    let mut before = lines[..header].to_vec();
    while before.last().is_some_and(|line| line.trim().is_empty()) {
        before.pop();
    }
    let mut out = before.concat();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&lines[header + 1..].concat());
    out
}

/// `text` with only taw's part of each older block taken out: the two markers and taw's own
/// `hooks = true`. Every other line stays; a block left with nothing but its header goes whole.
fn strip_markers_only(text: &str) -> String {
    let mut out = text.to_string();
    for range in toml_block_ranges(text).into_iter().rev() {
        let lines: Vec<&str> = text[range.clone()].lines().collect();
        let (mut in_table, mut dropped) = (false, false);
        let mut inner = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if index == 0 || index + 1 == lines.len() {
                continue;
            }
            if is_features_header(line) && !in_table && !dropped {
                in_table = true;
            } else if in_table && !dropped && is_hooks_true(line) {
                dropped = true;
                continue;
            } else if line.trim_start().starts_with('[') {
                in_table = false;
            }
            inner.push(*line);
        }
        if inner
            .iter()
            .all(|line| line.trim().is_empty() || is_features_header(line))
        {
            out = remove_toml_block_at(&out, range);
        } else {
            out.replace_range(range, &inner.join("\n"));
        }
    }
    out
}

/// What an install would still do to `config.toml` that Codex needs, in the words of a status.
fn config_incomplete(config: &Path) -> Option<Incomplete> {
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        // A config that cannot be read says nothing either way.
        Err(_) => return None,
    };
    let path = config.display();
    let reinstall = "taw integration install codex";
    let had_old = has_toml_block(&text);
    let cannot_load = || {
        Incomplete::Reinstall(format!(
            "Codex cannot load config.toml: an older taw's block is in it; a reinstall repairs \
             it. Run `{reinstall}` again."
        ))
    };
    let tidy = || {
        Incomplete::Reinstall(format!(
            "An older taw's block is in {path}; a reinstall tidies it. Run `{reinstall}` again."
        ))
    };
    match merged(&text) {
        Err(Refused {
            refusal: Refusal::NotToml { message, line },
            ..
        }) => Some(Incomplete::Waiting(format!(
            "Codex cannot load {path}: it does not parse ({message}, line {line}), so taw's hook \
             does not run. It is not taw's block that breaks it, so a reinstall would leave it \
             alone. Fix the file, then run `{reinstall}` again."
        ))),
        // A reinstall takes the older block out of the file, whatever it then refuses. Codex
        // cannot load the file only when the block is what broke it.
        Err(Refused {
            repaired: Some(_),
            broke,
            ..
        }) if had_old => Some(if broke { cannot_load() } else { tidy() }),
        Err(Refused {
            refusal: Refusal::HooksOff { line },
            ..
        }) => Some(Incomplete::Waiting(format!(
            "{path} sets hooks = false{}, so Codex does not run taw's hook. taw does not switch \
             it back: change it to hooks = true yourself.",
            line.map_or(String::new(), |line| format!(" on line {line}"))
        ))),
        Err(Refused {
            refusal: Refusal::HooksNotABool { found },
            ..
        }) => Some(Incomplete::Waiting(format!(
            "{path} sets hooks under [features] to a {found}, so Codex does not run taw's hook. \
             Change it to hooks = true yourself."
        ))),
        Err(Refused {
            refusal: refusal @ Refusal::Conflict { .. },
            ..
        }) => Some(Incomplete::Waiting(format!(
            "Codex cannot load {path}: {}.",
            refusal.detail("install")
        ))),
        // The refusal's own words: the shape found, its line, and where hooks = true goes.
        Err(Refused { refusal, .. }) => Some(Incomplete::Waiting(format!(
            "{path}: {}.",
            refusal.detail("install")
        ))),
        Ok(_) if had_old => {
            if text.parse::<toml::Table>().is_err() {
                Some(cannot_load())
            } else {
                Some(Incomplete::Reinstall(format!(
                    "An older taw wrote a delimited block into {path}; a reinstall replaces it \
                     with marked lines. Run `{reinstall}` again."
                )))
            }
        }
        Ok(_) => {
            let hooks_on = text
                .parse::<toml::Table>()
                .ok()
                .and_then(|table| table.get("features")?.get("hooks")?.as_bool())
                == Some(true);
            (!hooks_on).then(|| {
                Incomplete::Reinstall(format!(
                    "{path} has no hooks = true under [features], so Codex does not run taw's \
                     hook. Run `{reinstall}` again to add it."
                ))
            })
        }
    }
}

/// What an install and the status of an installed integration say of Codex's hook review: Codex holds
/// a new or changed hook until the user trusts it, and taw never writes that trust for them.
pub const HOOK_REVIEW: &str = "Codex asks once to review the new hook (Hooks need review): trust \
                               taw's hook there, or it never runs and codex sessions are not kept \
                               for a restore.";

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let hooks_json = hooks_json_path()?;
    let config_toml = config_toml_path()?;
    let needle = script.to_string_lossy().into_owned();
    // Both files are read and checked before either is edited or the script is published:
    // a config.toml this cannot use must not leave hooks.json already pointing at a hook
    // that was never finished.
    let hooks_edit = plan_json_hook(
        &hooks_json,
        &hooks_sidecar_path(&script),
        HOOKS_KEY,
        EVENT,
        &needle,
        hook_entry(&script),
    )?;
    let toml_edit = plan_text_edit_repairing(&config_toml, &toml_sidecar_path(&script), |text| {
        merged(text).map_err(|refused| {
            let (error, repaired) = refused.into_parts(&config_toml);
            super::Refused { error, repaired }
        })
    })?;

    write_executable(&script, &rendered)?;
    hooks_edit.apply()?;
    toml_edit
        .apply()
        .with_context(|| format!("updating {}", config_toml.display()))?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks_json, config_toml],
        message: format!("installed codex integration. {HOOK_REVIEW}"),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let hooks_json = hooks_json_path()?;
    let config_toml = config_toml_path()?;
    let needle = script.to_string_lossy().into_owned();

    // An older taw block that cannot be turned into the user's own lines (a key in both) is
    // reported before anything is removed, so the uninstall does not leave it behind unsaid.
    // Anything else it cannot convert still loses at least the markers and taw's own line.
    if let Ok(text) = std::fs::read_to_string(&config_toml)
        && let Err(refused) = convert(&text, false)
        && matches!(
            refused.refusal,
            Refusal::Conflict { .. } | Refusal::BlockKeys { .. }
        )
    {
        return Err(IntegrationError::UnusableConfig {
            path: config_toml.display().to_string(),
            detail: refused.refusal.detail("uninstall"),
        }
        .into());
    }
    uninstall_json_hook(
        &hooks_json,
        &hooks_sidecar_path(&script),
        HOOKS_KEY,
        EVENT,
        &needle,
    )?;
    // Only what an install of taw's put in: the marked line or the delimited block. A file
    // nothing else changed since is put back from the saved copy, byte for byte.
    uninstall_text_edit(
        &config_toml,
        &toml_sidecar_path(&script),
        strip_taw,
        |text| text.trim_end_matches(['\n', '\r']).to_string(),
    )?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks_json, config_toml],
        message: "uninstalled codex integration".to_string(),
    })
}

pub(super) fn status() -> Result<Status> {
    let script = script_path()?;
    let (installed, version) = match std::fs::read_to_string(&script) {
        Ok(contents) => (true, parse_version_marker(&contents, VERSION_MARKER_PREFIX)),
        Err(_) => (false, None),
    };
    let incomplete = if installed {
        config_incomplete(&config_toml_path()?)
    } else {
        None
    };
    Ok(Status {
        agent: AGENT.to_string(),
        installed,
        path: script,
        version,
        current: parse_version_marker(TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete,
    })
}
