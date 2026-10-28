//! `taw terminal title …` and `taw popup close` — the two verbs that act on the
//! *outer* terminal and the popup rather than on anything inside tmux.
//!
//! herdr draws its own UI, so it owns the terminal window title and any popup it opened.
//! tmux owns both here: the title comes from `set-titles` plus `set-titles-string`, and a
//! popup is closed with `display-popup -C`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::envelope::CliError;
use super::{Ctx, Sub};

/// Where the title `taw terminal title set` overwrote is kept, so `clear` can put it
/// back. Two globals rather than one, because `set-titles` and `set-titles-string` are
/// separate options and a user may have had either, both, or neither.
const SAVED_TITLES: &str = "@agent_wizard_saved_set_titles";
const SAVED_TITLES_STRING: &str = "@agent_wizard_saved_set_titles_string";

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Manage the outer terminal title
    Title {
        #[command(subcommand)]
        cmd: TitleCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum TitleCmd {
    /// Set the outer terminal title
    Set {
        #[arg(required = true, value_name = "TEXT")]
        text: Vec<String>,
    },
    /// Restore the title tmux was setting before `title set`
    Clear,
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Title { cmd } => match cmd {
            TitleCmd::Set { text } => ("title-set", set(ctx, &text.join(" "))),
            TitleCmd::Clear => ("title-clear", clear(ctx)),
        },
    }
}

/// Whether any client is attached.
///
/// A terminal title is something a *client's* terminal displays: with none attached there
/// is nothing to retitle, which is herdr's own `no_foreground_client` answer rather than
/// an error. The option is still written, so the title appears when a client attaches.
fn has_client(ctx: &Ctx) -> bool {
    ctx.tmux
        .list_clients()
        .map(|clients| !clients.is_empty())
        .unwrap_or(false)
}

/// The reply herdr's `client.window_title.*` gives: `{changed, reason}`.
fn reply(changed: bool, reason: &str, title: Option<&str>) -> Value {
    json!({ "changed": changed, "reason": reason, "title": title })
}

/// `terminal title set <text>` — turns `set-titles` on and points `set-titles-string` at
/// the literal text.
///
/// The text is written **literally**: `set-titles-string` is a tmux format string, so a
/// title containing `#{…}` or `#S` would otherwise be expanded into something else
/// entirely. `#` is the only escape tmux honours there, and `##` is a literal one.
///
/// Whatever the two options held first is saved on the first call, so `clear` restores
/// the user's own titling rather than tmux's default. A second `set` does not overwrite
/// the saved values — the thing to go back to is what the user had, not the previous
/// `taw` title.
fn set(ctx: &Ctx, text: &str) -> anyhow::Result<Value> {
    save_current(ctx)?;
    ctx.tmux
        .set_global_option("set-titles", "on")
        .map_err(|e| CliError::tmux(format!("enabling set-titles: {e:#}")))?;
    ctx.tmux
        .set_global_option("set-titles-string", &escape_format(text))
        .map_err(|e| CliError::tmux(format!("setting set-titles-string: {e:#}")))?;
    let reason = if has_client(ctx) {
        "set"
    } else {
        "no_foreground_client"
    };
    Ok(reply(true, reason, Some(text)))
}

/// `terminal title clear` — puts back whatever `set` overwrote.
///
/// With nothing saved (no `set` in this server's life) this reports `changed: false`
/// rather than resetting the options: clearing a title `taw` never set would silently
/// take away a titling scheme the user configured in their own `tmux.conf`.
fn clear(ctx: &Ctx) -> anyhow::Result<Value> {
    let saved_titles = global(ctx, SAVED_TITLES);
    let saved_string = global(ctx, SAVED_TITLES_STRING);
    if saved_titles.is_none() && saved_string.is_none() {
        return Ok(reply(false, "cleared", None));
    }
    restore(ctx, "set-titles", saved_titles.as_deref())?;
    restore(ctx, "set-titles-string", saved_string.as_deref())?;
    for marker in [SAVED_TITLES, SAVED_TITLES_STRING] {
        let _ = ctx.tmux.unset_global_option(marker);
    }
    let reason = if has_client(ctx) {
        "cleared"
    } else {
        "no_foreground_client"
    };
    Ok(reply(true, reason, None))
}

/// Records the current `set-titles`/`set-titles-string` under the markers, once.
///
/// The saved value is the *string* form of each option, with an empty option recorded as
/// the sentinel below — so "the user had `set-titles off`" and "the user had nothing" are
/// told apart on the way back.
fn save_current(ctx: &Ctx) -> anyhow::Result<()> {
    if global(ctx, SAVED_TITLES).is_some() || global(ctx, SAVED_TITLES_STRING).is_some() {
        return Ok(());
    }
    for (option, marker) in [
        ("set-titles", SAVED_TITLES),
        ("set-titles-string", SAVED_TITLES_STRING),
    ] {
        let current = ctx
            .tmux
            .get_global_option(option)
            .unwrap_or_default()
            .unwrap_or_default();
        let stored = if current.is_empty() {
            EMPTY_SENTINEL.to_string()
        } else {
            current
        };
        ctx.tmux
            .set_global_option(marker, &stored)
            .map_err(|e| CliError::tmux(format!("saving {option}: {e:#}")))?;
    }
    Ok(())
}

/// Stands in for "this option was empty when we saved it", so the marker's own presence
/// can mean "we have a saved value" without an empty string being indistinguishable from
/// an unset marker. Not a value tmux would ever produce for these two options.
const EMPTY_SENTINEL: &str = "\u{0}taw-empty";

fn restore(ctx: &Ctx, option: &str, saved: Option<&str>) -> anyhow::Result<()> {
    let Some(saved) = saved else {
        return Ok(());
    };
    if saved == EMPTY_SENTINEL {
        ctx.tmux
            .unset_global_option(option)
            .map_err(|e| CliError::tmux(format!("restoring {option} to its default: {e:#}")))
    } else {
        ctx.tmux
            .set_global_option(option, saved)
            .map_err(|e| CliError::tmux(format!("restoring {option}: {e:#}")))
    }
}

fn global(ctx: &Ctx, name: &str) -> Option<String> {
    ctx.tmux
        .get_global_option(name)
        .unwrap_or_default()
        .filter(|v| !v.is_empty())
}

/// Doubles every `#` so tmux's format expander leaves the text alone.
///
/// `set-titles-string` is expanded on every redraw. Without this, a title holding
/// `#{pane_current_command}` would show the command, and `#S` the session name — the
/// literal title the caller asked for would never appear.
pub(crate) fn escape_format(text: &str) -> String {
    text.replace('#', "##")
}

/// `taw popup close` — herdr's `popup.close`.
///
/// tmux closes a popup per *client* (`display-popup -C -c <client>`), and a popup is a
/// client-local overlay, so this closes one on every attached client rather than picking
/// one. With no client attached there is no popup, which is a success with `closed: 0`,
/// not an error: a plugin calling this to tidy up after itself must not fail because the
/// user has since detached.
pub(crate) fn popup_close(ctx: &Ctx) -> anyhow::Result<Value> {
    let clients = ctx
        .tmux
        .list_clients()
        .map_err(|e| CliError::tmux(format!("listing clients: {e:#}")))?;
    let mut closed = 0usize;
    for client in &clients {
        // `-C` closes whatever popup that client has open, and is a no-op when it has
        // none — so this counts clients asked, and cannot tell "closed one" from
        // "there was none" without a format tmux does not expose.
        if ctx
            .tmux
            .run_ok(&["display-popup", "-C", "-c", &client.client_name])
            .is_ok()
        {
            closed += 1;
        }
    }
    Ok(json!({ "clients": clients.len(), "closed": closed }))
}

/// `taw config reset-keys` — herdr's `config reset-keys`: drops the `[keys]` table from
/// `config.toml`, leaving every other byte of the file exactly as it was.
///
/// Byte-preserving on purpose, which is why this edits *lines* rather than round-tripping
/// the TOML. A `toml` parse and re-serialize would reformat the whole file and throw away
/// every comment in it — and `taw config default` ships a file that is mostly comments,
/// so a user's config is mostly comments too.
pub(crate) fn reset_keys(ctx: &Ctx) -> anyhow::Result<Value> {
    let path = &ctx.paths.config_file;
    if !path.exists() {
        return Ok(json!({
            "config_file": path,
            "removed": false,
            "note": "no config.toml, so there is no [keys] table to remove",
        }));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let (stripped, removed) = strip_keys_table(&raw);
    if !removed {
        return Ok(json!({ "config_file": path, "removed": false }));
    }
    // Written through a temp file and renamed, like every other file `taw` replaces: a
    // truncating write that failed halfway would leave the user with no config at all.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &stripped)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| anyhow::anyhow!("renaming {} to {}: {e}", tmp.display(), path.display()))?;
    Ok(json!({ "config_file": path, "removed": true }))
}

/// Removes the `[keys]` table and any `[keys.…]` subtable from a TOML document, keeping
/// every other line byte-for-byte. Returns the new text and whether anything was removed.
///
/// A table runs from its header line to the next top-level header, so this drops the
/// header and the lines under it. Comments *above* a `[keys]` header belong to it and go
/// too — a stranded "# keybindings, see the README" would otherwise sit above whatever
/// table came next and describe the wrong thing.
///
/// A BOM in front of a line (the first line of a file that starts with one, or one that
/// ended up mid-file) is not part of what the line says, since the loader drops it
/// (`config::normalize_utf8_bom`): [`visible`] looks past it to classify a line, and every
/// line that is kept is copied out exactly as it was, BOM included.
fn strip_keys_table(raw: &str) -> (String, bool) {
    let mut out: Vec<&str> = Vec::new();
    // Comment/blank lines held back until we know whether the next header is `[keys]`.
    let mut pending: Vec<&str> = Vec::new();
    let mut dropping = false;
    let mut removed = false;

    for line in raw.lines() {
        let trimmed = visible(line);
        if let Some(header) = table_header(trimmed) {
            let is_keys = header == "keys" || header.starts_with("keys.");
            if is_keys {
                // The comment block introducing it goes with it — but not the blank line
                // that separated it from whatever came before, which still belongs
                // between the two tables that end up adjacent.
                let keep = pending
                    .iter()
                    .rposition(|line| visible(line).is_empty())
                    .map_or(0, |i| i + 1);
                pending.truncate(keep);
                out.append(&mut pending);
                dropping = true;
                removed = true;
                continue;
            }
            out.append(&mut pending);
            dropping = false;
            out.push(line);
            continue;
        }
        if dropping {
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            pending.push(line);
        } else {
            out.append(&mut pending);
            out.push(line);
        }
    }
    out.append(&mut pending);

    let mut text = out.join("\n");
    // A TOML file ends in a newline; `lines()` dropped the last one.
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    (text, removed)
}

/// `line` as the TOML loader reads it: without a BOM in front, and without surrounding
/// whitespace.
fn visible(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim()
}

/// The dotted name inside a `[table]` header, or `None` for anything else. Array-of-table
/// headers (`[[x]]`) are not tables and are left alone — `[keys]` is never one.
fn table_header(trimmed: &str) -> Option<&str> {
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.starts_with('[') {
        return None;
    }
    Some(inner.trim())
}

/// Kept so `misc` can reuse the reply shape for `config reset-keys`'s envelope id.
pub(crate) const RESET_KEYS: Sub = "reset-keys";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_title_is_escaped_so_tmux_does_not_expand_it() {
        assert_eq!(escape_format("build: api"), "build: api");
        assert_eq!(
            escape_format("#{pane_current_command}"),
            "##{pane_current_command}"
        );
        assert_eq!(escape_format("#S / #W"), "##S / ##W");
        // An already-doubled hash is text too: the caller asked for two characters.
        assert_eq!(escape_format("a##b"), "a####b");
    }

    #[test]
    fn strip_keys_table_removes_the_table_and_keeps_every_other_byte() {
        let raw = "\
# top comment
[theme]
name = \"catppuccin\"

# these are the keybindings
[keys]
quit = \"q\"
focus = \"f\"

[worktrees]
directory = \"~/wt\"
";
        let (out, removed) = strip_keys_table(raw);
        assert!(removed);
        assert_eq!(
            out,
            "\
# top comment
[theme]
name = \"catppuccin\"

[worktrees]
directory = \"~/wt\"
"
        );
    }

    #[test]
    fn strip_keys_table_removes_subtables_too() {
        let raw = "\
[keys]
quit = \"q\"

[keys.agents]
open = \"o\"

[ui]
sidebar_width = 30
";
        let (out, removed) = strip_keys_table(raw);
        assert!(removed);
        assert_eq!(out, "[ui]\nsidebar_width = 30\n");
    }

    #[test]
    fn strip_keys_table_leaves_a_file_without_one_untouched_and_says_so() {
        let raw = "[theme]\nname = \"nord\"\n\n# a trailing comment\n";
        let (out, removed) = strip_keys_table(raw);
        assert!(!removed);
        assert_eq!(out, raw);
    }

    /// This edits the raw on-disk file line by line rather than
    /// round-tripping through `config::normalize_utf8_bom`, so it must not fabricate a
    /// BOM in a file that never had one, and must leave a displaced one on an untouched
    /// line exactly where it was — it only ever copies existing lines verbatim.
    #[test]
    fn strip_keys_table_neither_introduces_nor_removes_a_bom() {
        let raw = "[keys]\nq = \"q\"\n[ui]\n\u{feff}confirm_close = true\n";
        let (out, removed) = strip_keys_table(raw);
        assert!(removed, "[keys] must still be recognised and dropped");
        assert!(!out.contains("[keys]"));
        assert!(
            out.contains("\u{feff}confirm_close"),
            "an untouched line's own BOM must survive verbatim: {out:?}"
        );

        let clean = "[keys]\nq = \"q\"\n[ui]\nconfirm_close = true\n";
        let (out, _) = strip_keys_table(clean);
        assert!(!out.contains('\u{feff}'), "must never fabricate a BOM");
    }

    /// A config that starts with a BOM has one in front of its first line, and the loader
    /// accepts it (`config::normalize_utf8_bom`), so `[keys]` there is a real `[keys]`:
    /// `taw config reset-keys` finds it and removes it.
    #[test]
    fn strip_keys_table_finds_a_keys_table_on_a_line_that_starts_with_a_bom() {
        let raw = "\u{feff}[keys]\nq = \"q\"\n\n[ui]\nconfirm_close = true\n";
        let (out, removed) = strip_keys_table(raw);
        assert!(removed, "a BOM in front of the header must not hide it");
        assert_eq!(out, "[ui]\nconfirm_close = true\n");
    }

    /// The other way this went wrong: a header line that starts with a displaced BOM was
    /// not seen as a header, so the table after `[keys]` was taken to be part of it and
    /// deleted with it. It is a header, and the line is copied out with its BOM untouched.
    #[test]
    fn strip_keys_table_keeps_a_table_whose_header_line_starts_with_a_bom() {
        let raw = "[keys]\nq = \"q\"\n\u{feff}[worktrees]\ndirectory = \"~/wt\"\n";
        let (out, removed) = strip_keys_table(raw);
        assert!(removed);
        assert_eq!(out, "\u{feff}[worktrees]\ndirectory = \"~/wt\"\n");
    }

    #[test]
    fn strip_keys_table_does_not_confuse_a_table_whose_name_merely_starts_with_keys() {
        // `[keysomething]` is a different table; only `keys` and `keys.*` go.
        let raw = "[keysomething]\na = 1\n\n[keys]\nq = \"q\"\n";
        let (out, removed) = strip_keys_table(raw);
        assert!(removed);
        assert_eq!(out, "[keysomething]\na = 1\n");
    }

    #[test]
    fn strip_keys_table_ignores_an_array_of_tables_header() {
        let raw = "[[keys]]\non = \"x\"\n";
        let (out, removed) = strip_keys_table(raw);
        assert!(
            !removed,
            "`[[keys]]` is an array of tables, not the [keys] table"
        );
        assert_eq!(out, raw);
    }

    #[test]
    fn the_stripped_result_still_parses_as_toml() {
        let raw = "[keys]\nq = \"q\"\n\n[ui]\nconfirm_close = false\n";
        let (out, _) = strip_keys_table(raw);
        let table: toml::Table = toml::from_str(&out).unwrap();
        assert!(!table.contains_key("keys"));
        assert!(table.contains_key("ui"));
    }

    #[test]
    fn table_header_parses_only_a_bare_table_header() {
        assert_eq!(table_header("[keys]"), Some("keys"));
        assert_eq!(table_header("[ ui.sidebar ]"), Some("ui.sidebar"));
        assert_eq!(table_header("[[events]]"), None);
        assert_eq!(table_header("key = \"[keys]\""), None);
        assert_eq!(table_header("# [keys]"), None);
    }
}
