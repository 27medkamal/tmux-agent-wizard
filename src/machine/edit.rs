//! Adding a machine to `[machines]` and removing one, in the text of `config.toml`.
//!
//! The file is a person's: its comments, its order and its other tables stay as they are, so this edits
//! the text line by line and does not write the file out again from a parsed table. Where each line of
//! the file starts and ends (a value may run over several lines) is read by a small scanner; the scanner
//! can be wrong about a file written in an unusual way, so every edit is checked before it is returned: the
//! new text must parse, every other table must be what it was, the machines that were there must be the
//! same, and the loader that taw reads `[machines]` with must see the new machine (or no longer see the
//! removed one). An edit that fails the check is an error and the caller writes nothing.

use super::config::{bad_name, bad_taw, destination_fault, parse, same_destination, valid_name};

/// Why a machine could not be added to the text or removed from it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// What is asked is not a machine, in the words `taw config check` uses.
    #[error("{0}")]
    Invalid(String),
    /// The name is already listed.
    #[error("a machine named {0} is already listed")]
    Listed(String),
    /// The name is not listed.
    #[error("no machine named {0} is listed")]
    Unknown(String),
    /// The text is written in a way this does not edit, or does not parse.
    #[error("{0}")]
    NotEditable(String),
}

/// What the scanner makes of one statement of the file: a table header, or a `key = value` line.
#[derive(Debug)]
enum Kind {
    /// A table header, with its path when it is a plain table (`[machines.pi]` is `machines`, `pi`).
    Header(Option<Vec<String>>),
    /// A key and its value, with the first word of the key (`pi` for `pi.ssh = "x"`).
    Entry(Option<String>),
}

/// One statement: lines `first` to `last` (a value can run over several), as the scanner found them.
#[derive(Debug)]
struct Stmt {
    first: usize,
    last: usize,
    kind: Kind,
}

/// Where the scan of a line is: inside a multi-line string, and how deep in brackets.
#[derive(Default)]
struct Scan {
    depth: i32,
    multi: Option<char>,
}

fn scan_line(line: &str, state: &mut Scan) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(quote) = state.multi {
            if quote == '"' && c == '\\' {
                i += 2;
            } else if c == quote {
                let run = chars[i..].iter().take_while(|&&k| k == quote).count();
                i += run;
                if run >= 3 {
                    state.multi = None;
                }
            } else {
                i += 1;
            }
            continue;
        }
        match c {
            '#' => return,
            '"' | '\'' => {
                if chars[i..].iter().take_while(|&&k| k == c).count() >= 3 {
                    state.multi = Some(c);
                    i += 3;
                    continue;
                }
                i += 1;
                while i < chars.len() && chars[i] != c {
                    i += if c == '"' && chars[i] == '\\' { 2 } else { 1 };
                }
            }
            '[' | '{' => state.depth += 1,
            ']' | '}' => state.depth -= 1,
            _ => {}
        }
        i += 1;
    }
}

fn blank_or_comment(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.starts_with('#')
}

fn header_path(text: &str) -> Option<Vec<String>> {
    let mut table: toml::Table = toml::from_str(text).ok()?;
    let mut path = Vec::new();
    while table.len() == 1 {
        let (key, value) = table.into_iter().next()?;
        path.push(key);
        match value {
            toml::Value::Table(inner) => table = inner,
            _ => return None,
        }
    }
    (!path.is_empty()).then_some(path)
}

fn statements(lines: &[String]) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if blank_or_comment(&lines[i]) {
            i += 1;
            continue;
        }
        let first = i;
        let mut state = Scan::default();
        loop {
            scan_line(&lines[i], &mut state);
            if state.multi.is_none() && state.depth <= 0 {
                break;
            }
            if i + 1 >= lines.len() {
                break;
            }
            i += 1;
        }
        let text: String = lines[first..=i].concat();
        let kind = if text.trim_start().starts_with('[') {
            Kind::Header(header_path(&text))
        } else {
            let key = toml::from_str::<toml::Table>(&text)
                .ok()
                .and_then(|table| table.keys().next().cloned());
            Kind::Entry(key)
        };
        out.push(Stmt {
            first,
            last: i,
            kind,
        });
        i += 1;
    }
    out
}

/// The statements that make up the body of the table whose header is `stmts[h]`: up to the next header.
fn body(stmts: &[Stmt], h: usize) -> &[Stmt] {
    let end = stmts[h + 1..]
        .iter()
        .position(|s| matches!(s.kind, Kind::Header(_)))
        .map_or(stmts.len(), |n| h + 1 + n);
    &stmts[h + 1..end]
}

fn is_machines_header(stmt: &Stmt) -> bool {
    matches!(&stmt.kind, Kind::Header(Some(path)) if path == &["machines"])
}

fn in_machines_family(stmt: &Stmt) -> bool {
    matches!(&stmt.kind, Kind::Header(Some(path)) if path.first().is_some_and(|k| k == "machines"))
}

/// A string of the config, written the way a person would: in double quotes.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The text without its leading byte order mark (which is put back after the edit), and the lines of it
/// with their line ends.
fn split(text: &str) -> (&'static str, &str, Vec<String>) {
    let (bom, rest) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    let lines = rest.split_inclusive('\n').map(str::to_string).collect();
    (bom, rest, lines)
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn not_toml(err: &toml::de::Error) -> EditError {
    EditError::NotEditable(format!(
        "the config does not parse, so it is left as it is ({})",
        crate::config::one_line(&err.to_string())
    ))
}

fn machines_table(table: &toml::Table) -> Result<toml::Table, EditError> {
    match table.get("machines") {
        None => Ok(toml::Table::new()),
        Some(toml::Value::Table(machines)) => Ok(machines.clone()),
        Some(_) => Err(EditError::NotEditable(
            "machines is not a table of names and ssh destinations, so the config is left as it is"
                .to_string(),
        )),
    }
}

fn not_edited() -> EditError {
    EditError::NotEditable(
        "[machines] is written in a form this does not edit (a table as one value, or dotted keys \
         at the top of the file), so the config is left as it is"
            .to_string(),
    )
}

/// Checks the text an edit made against the text it started from. `added` is the machine a `taw machine
/// add` must have put there; none for a removal.
fn verify(
    before: &str,
    after: &str,
    name: &str,
    added: Option<(&str, Option<&str>)>,
) -> Result<(), EditError> {
    let mut old: toml::Table = toml::from_str(before).map_err(|e| not_toml(&e))?;
    let mut new: toml::Table = toml::from_str(after).map_err(|_| not_edited())?;
    let mut old_machines = machines_table(&old)?;
    let mut new_machines = machines_table(&new)?;
    old.remove("machines");
    new.remove("machines");
    let had = old_machines.remove(name).is_some();
    let has = new_machines.remove(name).is_some();
    if old != new || old_machines != new_machines {
        return Err(not_edited());
    }
    match added {
        None if had && !has => Ok(()),
        None => Err(not_edited()),
        Some((ssh, taw)) => {
            if had || !has {
                return Err(not_edited());
            }
            // The loader that reads the machines must find this one, and no line it would drop.
            let found = parse(after);
            if let Some(line) = found.dropped.first() {
                return Err(EditError::Invalid(format!(
                    "the [machines] table has a line taw drops, so nothing is added ({line})"
                )));
            }
            let listed = found
                .machines
                .iter()
                .any(|m| m.name == name && m.ssh == ssh && m.taw.as_deref() == taw);
            if listed { Ok(()) } else { Err(not_edited()) }
        }
    }
}

/// `text` with the machine `name` added, reached over `ssh`: `taw` is where taw is on it, when that is not
/// where a login shell finds it. A machine with no `taw` is one line under `[machines]` (a new table when
/// the file has none, at its end); one with it is a `[machines.<name>]` table with `ssh` and `taw`, beside
/// the other `[machines]` tables when there are some.
pub fn add(text: &str, name: &str, ssh: &str, taw: Option<&str>) -> Result<String, EditError> {
    if !valid_name(name) {
        return Err(EditError::Invalid(bad_name(name)));
    }
    if let Some(fault) = destination_fault(name, ssh) {
        return Err(EditError::Invalid(fault));
    }
    if taw.is_some_and(|path| path.is_empty() || path.chars().any(char::is_control)) {
        return Err(EditError::Invalid(bad_taw(name)));
    }
    let (bom, rest, mut lines) = split(text);
    let table: toml::Table = toml::from_str(rest).map_err(|e| not_toml(&e))?;
    if machines_table(&table)?.contains_key(name) {
        return Err(EditError::Listed(name.to_string()));
    }
    if let Some(other) = parse(rest).machines.iter().find(|m| m.ssh == ssh) {
        return Err(EditError::Invalid(same_destination(name, &other.name, ssh)));
    }

    let eol = if rest.contains("\r\n") { "\r\n" } else { "\n" };
    let block: Vec<String> = match taw {
        None => vec![format!("{name} = {}{eol}", quoted(ssh))],
        Some(path) => vec![
            format!("[machines.{name}]{eol}"),
            format!("ssh = {}{eol}", quoted(ssh)),
            format!("taw = {}{eol}", quoted(path)),
        ],
    };
    let stmts = statements(&lines);
    let table_end = |h: usize| body(&stmts, h).last().map_or(stmts[h].last, |s| s.last);
    let under = stmts.iter().position(is_machines_header);
    match (under, taw) {
        // One line under the table of that name, after its last statement.
        (Some(h), None) => {
            let at = table_end(h);
            if !lines[at].ends_with('\n') {
                lines[at].push_str(eol);
            }
            lines.splice(at + 1..at + 1, block);
        }
        // A table of its own, after the last of the tables of `[machines]`.
        (Some(_), Some(_)) => {
            let last = stmts
                .iter()
                .rposition(in_machines_family)
                .unwrap_or_default();
            let at = table_end(last);
            if !lines[at].ends_with('\n') {
                lines[at].push_str(eol);
            }
            let mut put = vec![eol.to_string()];
            put.extend(block);
            if lines.get(at + 1).is_some_and(|next| !is_blank(next)) {
                put.push(eol.to_string());
            }
            lines.splice(at + 1..at + 1, put);
        }
        // No table of that name: a new one at the end.
        (None, _) => {
            if let Some(last) = lines.last_mut()
                && !last.ends_with('\n')
            {
                last.push_str(eol);
            }
            if lines.last().is_some_and(|last| !is_blank(last)) {
                lines.push(eol.to_string());
            }
            if taw.is_none() {
                lines.push(format!("[machines]{eol}"));
            }
            lines.extend(block);
        }
    }
    let after = lines.concat();
    verify(rest, &after, name, Some((ssh, taw)))?;
    Ok(format!("{bom}{after}"))
}

/// `text` without the machine `name`: its line in `[machines]`, or its `[machines.<name>]` table (and the
/// blank lines after that table, when the table was set off by a blank line before it). Nothing else is
/// touched: not the comments, not the other machines, not a `[machines]` header left with none.
pub fn remove(text: &str, name: &str) -> Result<String, EditError> {
    let (bom, rest, mut lines) = split(text);
    let table: toml::Table = toml::from_str(rest).map_err(|e| not_toml(&e))?;
    if !machines_table(&table)?.contains_key(name) {
        return Err(EditError::Unknown(name.to_string()));
    }
    let stmts = statements(&lines);
    // Line ranges to delete, last first.
    let mut cut: Vec<(usize, usize)> = Vec::new();
    let mut h = 0;
    while h < stmts.len() {
        match &stmts[h].kind {
            Kind::Header(Some(path))
                if path.len() >= 2 && path[0] == "machines" && path[1] == name =>
            {
                let end = body(&stmts, h).last().map_or(stmts[h].last, |s| s.last);
                let mut end_line = end;
                if stmts[h].first == 0 || is_blank(&lines[stmts[h].first - 1]) {
                    while lines.get(end_line + 1).is_some_and(|next| is_blank(next)) {
                        end_line += 1;
                    }
                }
                cut.push((stmts[h].first, end_line));
            }
            Kind::Header(Some(path)) if path == &["machines"] => {
                for stmt in body(&stmts, h) {
                    if matches!(&stmt.kind, Kind::Entry(Some(key)) if key == name) {
                        cut.push((stmt.first, stmt.last));
                    }
                }
            }
            _ => {}
        }
        h += 1;
    }
    cut.sort_unstable();
    for (first, last) in cut.into_iter().rev() {
        lines.drain(first..=last);
    }
    let after = lines.concat();
    verify(rest, &after, name, None)?;
    Ok(format!("{bom}{after}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOMY: &str = "# my config\nonboarding = false\n\n[theme]\nname = \"x\" # keep this\n\n\
        # Other machines\n[machines]\n# studio = \"studio.local\"\npi = \"pi\" # the pi\n\n\
        # after\n[plugins]\nx = 1\n";

    fn names(text: &str) -> Vec<String> {
        parse(text).machines.into_iter().map(|m| m.name).collect()
    }

    #[test]
    fn a_short_machine_goes_under_the_table_after_its_last_line() {
        let got = add(ROOMY, "studio", "studio.local", None).unwrap();
        let want = ROOMY.replace(
            "pi = \"pi\" # the pi\n",
            "pi = \"pi\" # the pi\nstudio = \"studio.local\"\n",
        );
        assert_eq!(got, want);
        assert_eq!(names(&got), ["pi", "studio"]);
    }

    #[test]
    fn a_machine_with_taw_is_a_table_of_its_own_beside_the_machines_table() {
        let got = add(ROOMY, "den", "den", Some("~/.local/bin/taw")).unwrap();
        let want = ROOMY.replace(
            "pi = \"pi\" # the pi\n",
            "pi = \"pi\" # the pi\n\n[machines.den]\nssh = \"den\"\ntaw = \"~/.local/bin/taw\"\n",
        );
        assert_eq!(got, want);
        let found = parse(&got).machines;
        assert_eq!(found[1].taw.as_deref(), Some("~/.local/bin/taw"));
    }

    #[test]
    fn a_table_that_follows_directly_keeps_a_blank_line_before_it() {
        let text = "[machines]\npi = \"pi\"\n[plugins]\nx = 1\n";
        let got = add(text, "den", "den", Some("/bin/taw")).unwrap();
        assert_eq!(
            got,
            "[machines]\npi = \"pi\"\n\n[machines.den]\nssh = \"den\"\ntaw = \"/bin/taw\"\n\n[plugins]\nx = 1\n"
        );
    }

    #[test]
    fn with_no_table_a_new_one_is_added_at_the_end() {
        let text = "# nothing here\n[theme]\nname = \"x\"\n";
        assert_eq!(
            add(text, "pi", "pi", None).unwrap(),
            "# nothing here\n[theme]\nname = \"x\"\n\n[machines]\npi = \"pi\"\n"
        );
        assert_eq!(
            add(text, "pi", "pi", Some("~/taw")).unwrap(),
            "# nothing here\n[theme]\nname = \"x\"\n\n[machines.pi]\nssh = \"pi\"\ntaw = \"~/taw\"\n"
        );
    }

    #[test]
    fn a_file_with_no_final_newline_gets_one_before_the_new_table() {
        assert_eq!(
            add("a = 1", "pi", "pi", None).unwrap(),
            "a = 1\n\n[machines]\npi = \"pi\"\n"
        );
    }

    #[test]
    fn a_missing_or_empty_file_becomes_just_that_table() {
        assert_eq!(
            add("", "studio", "studio.local", None).unwrap(),
            "[machines]\nstudio = \"studio.local\"\n"
        );
        assert_eq!(
            add("", "pi", "pi", Some("~/taw")).unwrap(),
            "[machines.pi]\nssh = \"pi\"\ntaw = \"~/taw\"\n"
        );
    }

    #[test]
    fn a_commented_example_stays_a_comment_and_the_table_goes_to_the_end() {
        let text = "[sandbox]\n# profile = \"default\"\n\n# [machines]\n# studio = \"studio.local\"\n\n\
                    [plugins]\nx = 1\n";
        let got = add(text, "studio", "studio.local", None).unwrap();
        assert_eq!(
            got,
            format!("{text}\n[machines]\nstudio = \"studio.local\"\n")
        );
        assert!(got.contains("\n# [machines]\n# studio = \"studio.local\"\n"));
    }

    #[test]
    fn the_shipped_default_config_takes_a_machine_at_its_end() {
        let text = crate::config::DEFAULT_TOML;
        let got = add(text, "studio", "studio.local", None).unwrap();
        assert!(got.starts_with(text));
        assert_eq!(names(&got), ["studio"]);
    }

    #[test]
    fn a_line_inside_a_multi_line_string_is_not_a_table() {
        let text = "[hooks]\nx = \"\"\"\n[machines]\npi = \"pi\"\n\"\"\"\ny = [\n  [1, 2],\n]\n";
        let got = add(text, "den", "den", None).unwrap();
        assert_eq!(got, format!("{text}\n[machines]\nden = \"den\"\n"));
    }

    #[test]
    fn a_crlf_file_keeps_its_line_ends_and_a_leading_bom() {
        let text = "\u{feff}[machines]\r\npi = \"pi\"\r\n";
        let got = add(text, "den", "den", None).unwrap();
        assert_eq!(
            got,
            "\u{feff}[machines]\r\npi = \"pi\"\r\nden = \"den\"\r\n"
        );
        let got = add(text, "den", "den", Some("/t")).unwrap();
        assert_eq!(
            got,
            "\u{feff}[machines]\r\npi = \"pi\"\r\n\r\n[machines.den]\r\nssh = \"den\"\r\ntaw = \"/t\"\r\n"
        );
    }

    #[test]
    fn a_name_already_listed_is_refused_in_either_form() {
        let text = "[machines]\npi = \"pi\"\n[machines.den]\nssh = \"den\"\n";
        for name in ["pi", "den"] {
            assert_eq!(
                add(text, name, "elsewhere", None),
                Err(EditError::Listed(name.to_string()))
            );
        }
        assert_eq!(
            add(text, "pi", "x", None).unwrap_err().to_string(),
            "a machine named pi is already listed"
        );
    }

    #[test]
    fn a_name_or_destination_that_cannot_be_one_is_refused_in_the_words_of_config_check() {
        assert_eq!(
            add("", "Studio", "h", None),
            Err(EditError::Invalid(bad_name("Studio")))
        );
        assert!(
            add("", "x", "-oProxyCommand=y", None)
                .unwrap_err()
                .to_string()
                .contains("cannot start with -")
        );
        assert!(
            add("", "x", "a b", None)
                .unwrap_err()
                .to_string()
                .contains("no spaces")
        );
        assert!(
            add("", "x", "", None)
                .unwrap_err()
                .to_string()
                .contains("no ssh destination")
        );
        assert_eq!(
            add("", "x", "h", Some("")),
            Err(EditError::Invalid(bad_taw("x")))
        );
        assert_eq!(
            add("", "x", "h", Some("a\nb")),
            Err(EditError::Invalid(bad_taw("x")))
        );
    }

    #[test]
    fn a_destination_another_machine_has_is_refused() {
        let text = "[machines]\npi = \"pi\"\n";
        assert_eq!(
            add(text, "pi2", "pi", None),
            Err(EditError::Invalid(same_destination("pi2", "pi", "pi")))
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_left_alone() {
        let err = add("[machines\npi = \n", "den", "den", None).unwrap_err();
        assert!(matches!(err, EditError::NotEditable(_)), "{err}");
    }

    #[test]
    fn a_table_written_as_an_inline_value_is_refused_not_broken() {
        let text = "machines = { pi = \"pi\" }\n";
        let err = add(text, "den", "den", None).unwrap_err();
        assert!(matches!(err, EditError::NotEditable(_)), "{err}");
        let err = remove(text, "pi").unwrap_err();
        assert!(matches!(err, EditError::NotEditable(_)), "{err}");
    }

    #[test]
    fn a_table_with_a_line_taw_drops_is_not_added_to() {
        let err = add("[machines]\nattic = \"\"\n", "den", "den", None).unwrap_err();
        assert!(err.to_string().contains("attic"), "{err}");
    }

    #[test]
    fn a_short_machine_is_removed_with_its_line_and_nothing_else() {
        let text = "[machines]\n# studio = \"studio.local\"\nstudio = \"studio.local\" # box\npi = \"pi\"\n\n[plugins]\nx = 1\n";
        let got = remove(text, "studio").unwrap();
        assert_eq!(
            got,
            "[machines]\n# studio = \"studio.local\"\npi = \"pi\"\n\n[plugins]\nx = 1\n"
        );
    }

    #[test]
    fn a_table_machine_is_removed_with_its_table_and_one_blank_line() {
        let text = "[machines]\nstudio = \"s\"\n\n[machines.pi]\nssh = \"pi\"\n# note\ntaw = \"~/taw\"\n\n[plugins]\nx = 1\n";
        let got = remove(text, "pi").unwrap();
        assert_eq!(got, "[machines]\nstudio = \"s\"\n\n[plugins]\nx = 1\n");
        assert_eq!(names(&got), ["studio"]);
    }

    #[test]
    fn the_last_table_machine_goes_without_leaving_a_blank_run() {
        let text = "[theme]\na = 1\n\n[machines.pi]\nssh = \"pi\"\n";
        assert_eq!(remove(text, "pi").unwrap(), "[theme]\na = 1\n\n");
    }

    #[test]
    fn removing_the_only_machine_leaves_the_table_header() {
        assert_eq!(
            remove("[machines]\npi = \"pi\"\n", "pi").unwrap(),
            "[machines]\n"
        );
    }

    #[test]
    fn a_name_that_is_not_listed_is_refused() {
        let text = "[machines]\npi = \"pi\"\n# den = \"den\"\n";
        assert_eq!(
            remove(text, "den"),
            Err(EditError::Unknown("den".to_string()))
        );
        assert_eq!(
            remove("", "den").unwrap_err().to_string(),
            "no machine named den is listed"
        );
    }

    #[test]
    fn add_then_remove_gives_the_file_back() {
        for taw in [None, Some("~/taw")] {
            let added = add(ROOMY, "den", "den", taw).unwrap();
            assert_eq!(remove(&added, "den").unwrap(), ROOMY, "{taw:?}");
        }
        let bare = "# c\n[theme]\na = 1\n";
        let added = add(bare, "den", "den", None).unwrap();
        assert_eq!(
            remove(&added, "den").unwrap(),
            format!("{bare}\n[machines]\n")
        );
        let added = add(bare, "den", "den", Some("~/taw")).unwrap();
        assert_eq!(remove(&added, "den").unwrap(), format!("{bare}\n"));
    }

    #[test]
    fn the_valid_name_rule_is_the_one_the_config_check_applies() {
        assert!(valid_name("pi-2") && !valid_name("Pi"));
    }
}
