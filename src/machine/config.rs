//! `[machines]` of `config.toml`: the names taw calls other machines by, and the ssh destination of
//! each, read in the order the file lists them.
//!
//! The table is read on its own, in the way `taw sandbox` reads `[sandbox]`: whole. A line taw would
//! drop (a bad name, a destination that cannot be one, a key it does not know) makes
//! [`load_machines`] refuse the table, so that a machine is never run on half of what was written. The
//! words of each dropped line are made here, once, for `taw config check` to say as well.

use crate::paths::Paths;

use super::Machine;

/// What [`parse`] found in the `[machines]` table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// The machines that are usable, in the order of the file.
    pub machines: Vec<Machine>,
    /// A line for each thing taw dropped, in the words `taw config check` uses.
    pub dropped: Vec<String>,
}

/// Why the machines cannot be told from `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MachinesDoubt {
    /// The file does not load: why, on one line.
    #[error("config {file} could not be read ({why})")]
    NotLoaded { file: String, why: String },
    /// Its `[machines]` table has a line taw drops, in config check's words.
    #[error("config {file} has a line in [machines] taw drops ({line})")]
    Dropped { file: String, line: String },
}

/// The longest a machine's name may be.
const NAME_MAX: usize = 32;

/// Whether `name` can name a machine: lowercase letters, digits, `-` and `_`, starting with a letter,
/// at most 32 characters. It is what the headers, the toasts and `--machine` call the machine, and
/// it is the name of a file and of an ssh control socket, so nothing else gets through.
pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// `text` as one line a person reads: each control character is written out (a newline as `\n`, escape as
/// `\u{1b}`, any other as `\u{..}`), and every other character as it is. A name or a key is whatever the
/// file holds, and a raw escape sequence in a message would be run by the terminal that shows it.
fn shown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn bad_name(name: &str) -> String {
    let name = shown(name);
    format!(
        "machines.{name}: a machine's name is lowercase letters, digits, - and _, starting with a \
         letter, at most {NAME_MAX} characters"
    )
}

fn no_destination(name: &str) -> String {
    let name = shown(name);
    format!("machines.{name}: no ssh destination (what you type after ssh)")
}

fn dash_destination(name: &str) -> String {
    let name = shown(name);
    format!("machines.{name}: an ssh destination cannot start with -")
}

fn spaced_destination(name: &str) -> String {
    let name = shown(name);
    format!("machines.{name}: an ssh destination has no spaces")
}

pub(crate) fn same_destination(name: &str, other: &str, destination: &str) -> String {
    let (name, other) = (shown(name), shown(other));
    format!("machines.{name}: the same ssh destination as machines.{other} ({destination})")
}

fn neither(name: &str) -> String {
    let name = shown(name);
    format!("machines.{name}: an ssh destination (\"studio.local\") or a table with ssh and taw")
}

pub(crate) fn bad_taw(name: &str) -> String {
    let name = shown(name);
    format!("machines.{name}.taw: a path to taw on that machine, ~/ for its home")
}

fn unknown_key(name: &str, key: &str) -> String {
    let (name, key) = (shown(name), shown(key));
    format!("unknown config key: machines.{name}.{key}")
}

const NOT_A_TABLE: &str = "machines: a table of names and ssh destinations";

/// What is wrong with a destination, in the words of the dropped line, or `None` when it is one.
pub(crate) fn destination_fault(name: &str, destination: &str) -> Option<String> {
    if destination.is_empty() {
        Some(no_destination(name))
    } else if destination.starts_with('-') {
        Some(dash_destination(name))
    } else if destination
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        Some(spaced_destination(name))
    } else {
        None
    }
}

/// One entry of the table, read: the machine, when it can be used, and the lines dropped on the way.
fn read_entry(
    name: &str,
    value: &toml::de::DeValue<'_>,
) -> (Option<(String, Option<String>)>, Vec<String>) {
    use toml::de::DeValue;
    match value {
        DeValue::String(destination) => (Some((destination.to_string(), None)), Vec::new()),
        DeValue::Table(table) => {
            let mut dropped = Vec::new();
            let mut ssh: Option<String> = None;
            let mut taw: Option<String> = None;
            let mut taw_bad = false;
            for (key, value) in table.iter() {
                match (key.get_ref().as_ref(), value.get_ref()) {
                    ("ssh", DeValue::String(destination)) => ssh = Some(destination.to_string()),
                    ("taw", DeValue::String(path)) if !path.is_empty() => {
                        taw = Some(path.to_string());
                    }
                    ("taw", _) => taw_bad = true,
                    // An `ssh` that is not a string is no destination at all.
                    ("ssh", _) => {}
                    (other, _) => dropped.push(unknown_key(name, other)),
                }
            }
            if taw_bad {
                dropped.push(bad_taw(name));
                return (None, dropped);
            }
            (Some((ssh.unwrap_or_default(), taw)), dropped)
        }
        _ => (None, vec![neither(name)]),
    }
}

/// Reads the `[machines]` table of a config file's text, and nothing else of it. A text that is not
/// TOML has no machines and drops nothing here: [`load_machines`] is the one that says it is not read.
///
/// The order is the order of the file, which `toml::Table` (a sorted map) does not keep: the keys of
/// the table are put in the order of where each is written, by the span the parser gives them.
pub fn parse(text: &str) -> Parsed {
    let text = crate::config::normalize_utf8_bom(text);
    let Ok(document) = toml::de::DeTable::parse(&text) else {
        return Parsed::default();
    };
    let Some((_, table)) = document
        .get_ref()
        .iter()
        .find(|(key, _)| key.get_ref().as_ref() == "machines")
    else {
        return Parsed::default();
    };
    let toml::de::DeValue::Table(entries) = table.get_ref() else {
        return Parsed {
            machines: Vec::new(),
            dropped: vec![NOT_A_TABLE.to_string()],
        };
    };
    let mut entries: Vec<_> = entries.iter().collect();
    entries.sort_by_key(|(key, _)| key.span().start);

    let mut parsed = Parsed::default();
    for (key, value) in entries {
        let name = key.get_ref().to_string();
        if !valid_name(&name) {
            parsed.dropped.push(bad_name(&name));
            continue;
        }
        let (found, dropped) = read_entry(&name, value.get_ref());
        parsed.dropped.extend(dropped);
        let Some((ssh, taw)) = found else { continue };
        if let Some(fault) = destination_fault(&name, &ssh) {
            parsed.dropped.push(fault);
            continue;
        }
        if let Some(first) = parsed.machines.iter().find(|m| m.ssh == ssh) {
            parsed
                .dropped
                .push(same_destination(&name, &first.name, &ssh));
            continue;
        }
        parsed.machines.push(Machine { name, ssh, taw });
    }
    parsed
}

/// The config file as a person is told where it is: with `~` for their home folder.
pub(crate) fn config_file_word(paths: &Paths) -> String {
    match dirs::home_dir() {
        Some(home) => crate::session::naming::tilde(&paths.config_file, &home),
        None => paths.config_file.display().to_string(),
    }
}

/// The machines of `config.toml`, in the order it lists them; none for a file that is not there or has
/// no `[machines]` table. Refused ([`MachinesDoubt`]) when taw cannot tell which machines you listed:
/// the file does not parse, or the table has a line taw drops.
pub fn load_machines(paths: &Paths) -> Result<Vec<Machine>, MachinesDoubt> {
    let path = &paths.config_file;
    let file = config_file_word(paths);
    let not_loaded = |why: String| MachinesDoubt::NotLoaded {
        file: file.clone(),
        why: crate::config::one_line(&why),
    };
    // Only a file that is not there at all is no config; a link whose file is gone is one taw
    // cannot read.
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(not_loaded(err.to_string())),
        Ok(_) => {}
    }
    let raw = std::fs::read_to_string(path).map_err(|err| not_loaded(err.to_string()))?;
    let raw = crate::config::normalize_utf8_bom(&raw);
    if let Err(err) = toml::from_str::<toml::Table>(&raw) {
        return Err(not_loaded(err.to_string()));
    }
    let parsed = parse(&raw);
    match parsed.dropped.into_iter().next() {
        Some(line) => Err(MachinesDoubt::Dropped { file, line }),
        None => Ok(parsed.machines),
    }
}

/// What one look at the config file saw: which file it is (its inode), when it was last changed, how long it is
/// and when its status last changed, or `None` when it is not there (or cannot be looked at). The inode and the
/// change time are there because a file renamed into place from a copy that kept its modified time and length
/// (`cp -p`, `mv config.toml.bak config.toml`) is a change too, and no tool sets a change time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp(Option<Seen>);

/// The parts of a look at a file that are not nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    modified: std::time::SystemTime,
    len: u64,
    inode: u64,
    changed: (i64, i64),
}

/// One look at the config file: a single metadata call, which follows a link.
pub fn stamp_of(paths: &Paths) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp(std::fs::metadata(&paths.config_file).ok().and_then(|meta| {
        Some(Seen {
            modified: meta.modified().ok()?,
            len: meta.len(),
            inode: meta.ino(),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }))
}

/// Notices that the config file changed, and waits for the change to settle: a file an editor is still writing
/// is not read until two looks in a row find it the same.
#[derive(Debug)]
pub struct ConfigWatch {
    /// The look at the file the machines now held were read from.
    read: Stamp,
    /// What `read` was before the last settled look took its place, for [`ConfigWatch::read_was_of`].
    before: Stamp,
    /// The look before this one.
    last: Stamp,
}

impl ConfigWatch {
    /// `first` is the look made before the machines were first read.
    pub fn new(first: Stamp) -> ConfigWatch {
        ConfigWatch {
            read: first,
            before: first,
            last: first,
        }
    }

    /// Takes in a look. `true` once for each change, when this look finds the file as the one before did: the
    /// file is to be read now.
    pub fn settled(&mut self, look: Stamp) -> bool {
        let settled = look != self.read && look == self.last;
        if settled {
            self.before = self.read;
            self.read = look;
        }
        self.last = look;
        settled
    }

    /// Whether the file read just now was the settled one: `after` is a look made after the read. When it is not
    /// (an editor began writing in between, and what was read may be half a file) the read is not to be used, and
    /// the change counts as not yet read: it is read again once the file has settled again.
    pub fn read_was_of(&mut self, after: Stamp) -> bool {
        if after == self.read {
            return true;
        }
        self.read = self.before;
        self.last = after;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    fn machine(name: &str, ssh: &str, taw: Option<&str>) -> Machine {
        Machine {
            name: name.to_string(),
            ssh: ssh.to_string(),
            taw: taw.map(str::to_string),
        }
    }

    fn dropped_of(text: &str) -> Vec<String> {
        parse(text).dropped
    }

    #[test]
    fn the_short_form_and_the_long_form_are_the_same_setting() {
        let short = parse("[machines]\nstudio = \"studio.local\"\n");
        let long = parse("[machines.studio]\nssh = \"studio.local\"\n");
        assert_eq!(
            short.machines,
            vec![machine("studio", "studio.local", None)]
        );
        assert_eq!(short.machines, long.machines);
        assert!(short.dropped.is_empty() && long.dropped.is_empty());
    }

    #[test]
    fn the_long_form_carries_the_path_of_taw() {
        let got = parse("[machines.pi]\nssh = \"pi\"\ntaw = \"~/.local/bin/taw\"\n");
        assert_eq!(
            got.machines,
            vec![machine("pi", "pi", Some("~/.local/bin/taw"))]
        );
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn the_order_of_the_table_is_the_order_of_the_file() {
        let got = parse(
            "[machines]\npi = \"pi\"\nstudio = \"studio.local\"\nattic = \"attic\"\n\
             [machines.zeta]\nssh = \"zeta\"\n",
        );
        let names: Vec<&str> = got.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["pi", "studio", "attic", "zeta"]);
    }

    #[test]
    fn a_long_form_between_short_forms_keeps_its_place() {
        let got = parse(
            "[machines]\nzeta = \"z\"\n[machines.pi]\nssh = \"pi\"\n\n[machines.alpha]\nssh = \"a\"\n",
        );
        let names: Vec<&str> = got.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["zeta", "pi", "alpha"]);
    }

    #[test]
    fn a_name_is_lowercase_letters_digits_dash_and_underscore_from_a_letter_up_to_32() {
        let says = |name: &str| {
            format!(
                "machines.{name}: a machine's name is lowercase letters, digits, - and _, \
                 starting with a letter, at most 32 characters"
            )
        };
        let thirty_three = "a".repeat(33);
        for bad in [
            "Studio",
            "2pi",
            "-x",
            "_x",
            "x.y",
            "x y",
            "pi\u{e9}",
            thirty_three.as_str(),
        ] {
            let text = format!("[machines]\n\"{bad}\" = \"host\"\n");
            let got = parse(&text);
            assert!(got.machines.is_empty(), "{bad} was kept");
            assert_eq!(got.dropped, vec![says(bad)], "{bad}");
        }
        let thirty_two = "a".repeat(32);
        for good in ["pi-2", "a_b", "p", thirty_two.as_str()] {
            let text = format!("[machines]\n\"{good}\" = \"host\"\n");
            let got = parse(&text);
            assert_eq!(got.machines, vec![machine(good, "host", None)], "{good}");
            assert!(got.dropped.is_empty(), "{good}");
        }
    }

    #[test]
    fn an_empty_destination_is_dropped() {
        let line = "machines.studio: no ssh destination (what you type after ssh)".to_string();
        for text in [
            "[machines]\nstudio = \"\"\n",
            "[machines.studio]\ntaw = \"~/bin/taw\"\n",
            "[machines.studio]\nssh = \"\"\n",
        ] {
            let got = parse(text);
            assert!(got.machines.is_empty(), "{text}");
            assert_eq!(got.dropped, vec![line.clone()], "{text}");
        }
    }

    #[test]
    fn a_destination_cannot_start_with_a_dash() {
        let got = parse("[machines]\nstudio = \"-oProxyCommand=x\"\n");
        assert!(got.machines.is_empty());
        assert_eq!(
            got.dropped,
            vec!["machines.studio: an ssh destination cannot start with -"]
        );
    }

    #[test]
    fn a_destination_with_a_space_or_a_control_character_is_dropped() {
        for dest in [
            "studio local",
            "studio\\tlocal",
            "studio\\u0007",
            "studio\\n",
            " studio",
        ] {
            let text = format!("[machines]\nstudio = \"{dest}\"\n");
            let got = parse(&text);
            assert!(got.machines.is_empty(), "{dest}");
            assert_eq!(
                got.dropped,
                vec!["machines.studio: an ssh destination has no spaces"],
                "{dest}"
            );
        }
    }

    #[test]
    fn the_later_of_two_machines_with_one_destination_is_dropped() {
        let got = parse("[machines]\nstudio = \"box.local\"\npi = \"pi\"\nattic = \"box.local\"\n");
        let names: Vec<&str> = got.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["studio", "pi"]);
        assert_eq!(
            got.dropped,
            vec!["machines.attic: the same ssh destination as machines.studio (box.local)"]
        );
    }

    #[test]
    fn an_unknown_key_in_a_long_form_is_reported_and_the_machine_stays() {
        let got = parse("[machines.pi]\nssh = \"pi\"\nport = 22\n");
        assert_eq!(got.machines, vec![machine("pi", "pi", None)]);
        assert_eq!(got.dropped, vec!["unknown config key: machines.pi.port"]);
    }

    const BAD_NAME_WORDS: &str = "a machine's name is lowercase letters, digits, - and _, starting \
                                  with a letter, at most 32 characters";

    #[test]
    fn a_name_or_key_with_a_control_character_is_shown_escaped_in_the_dropped_line() {
        let got = dropped_of("[machines]\n\"pi\\n2\" = \"x\"\n");
        assert_eq!(got, vec![format!("machines.pi\\n2: {BAD_NAME_WORDS}")]);
        let got = dropped_of("[machines]\n\"x\\u001b[2J\" = \"x\"\n");
        assert_eq!(
            got,
            vec![format!("machines.x\\u{{1b}}[2J: {BAD_NAME_WORDS}")]
        );
        let got = dropped_of("[machines]\n\"a\\tb\\rc\" = \"x\"\n");
        assert_eq!(
            got,
            vec![format!("machines.a\\u{{9}}b\\u{{d}}c: {BAD_NAME_WORDS}")]
        );
        let got = dropped_of("[machines.pi]\nssh = \"pi\"\n\"k\\ney\" = 1\n\"k\\u001bz\" = 2\n");
        assert_eq!(
            got,
            vec![
                "unknown config key: machines.pi.k\\ney",
                "unknown config key: machines.pi.k\\u{1b}z"
            ]
        );
        // Everything else is shown as written, accents and symbols included.
        let got = dropped_of("[machines]\n\"Caf\u{e9} \u{2603}\" = \"x\"\n");
        assert_eq!(
            got,
            vec![format!("machines.Caf\u{e9} \u{2603}: {BAD_NAME_WORDS}")]
        );
    }

    #[test]
    fn the_doubt_says_a_control_character_escaped_too() {
        let (_dir, paths) = paths_with(Some("[machines]\n\"pi\\n2\" = \"x\"\n"));
        let said = load_machines(&paths).unwrap_err().to_string();
        assert!(
            said.ends_with(&format!("(machines.pi\\n2: {BAD_NAME_WORDS})")),
            "{said}"
        );
        assert!(!said.contains('\n'), "{said:?}");
    }

    #[test]
    fn a_value_that_is_neither_a_string_nor_a_table_is_dropped() {
        for value in ["3", "true", "[\"a\", \"b\"]", "1.5"] {
            let text = format!("[machines]\nstudio = {value}\n");
            let got = parse(&text);
            assert!(got.machines.is_empty(), "{value}");
            assert_eq!(
                got.dropped,
                vec![
                    "machines.studio: an ssh destination (\"studio.local\") or a table with ssh and taw"
                ],
                "{value}"
            );
        }
    }

    #[test]
    fn a_taw_that_is_not_a_non_empty_string_drops_the_machine() {
        for value in ["\"\"", "3", "[\"x\"]"] {
            let text = format!("[machines.pi]\nssh = \"pi\"\ntaw = {value}\n");
            let got = parse(&text);
            assert!(got.machines.is_empty(), "{value}");
            assert_eq!(
                got.dropped,
                vec!["machines.pi.taw: a path to taw on that machine, ~/ for its home"],
                "{value}"
            );
        }
    }

    #[test]
    fn machines_that_is_not_a_table_is_dropped() {
        let got = parse("machines = \"studio.local\"\n");
        assert!(got.machines.is_empty());
        assert_eq!(
            got.dropped,
            vec!["machines: a table of names and ssh destinations"]
        );
    }

    #[test]
    fn a_file_without_the_table_has_no_machines_and_drops_nothing() {
        let got = parse("[ui]\ntheme = \"dark\"\n");
        assert!(got.machines.is_empty() && got.dropped.is_empty());
        let got = parse("");
        assert!(got.machines.is_empty() && got.dropped.is_empty());
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_table() {
        let got = parse("\u{feff}[machines]\nstudio = \"studio.local\"\n");
        assert_eq!(got.machines, vec![machine("studio", "studio.local", None)]);
    }

    #[test]
    fn the_other_tables_of_the_file_are_not_looked_at() {
        let got = parse("[remote]\nfoo = 1\n[server]\nbar = 2\n[machines]\npi = \"pi\"\n");
        assert_eq!(got.machines, vec![machine("pi", "pi", None)]);
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn dropped_lines_come_in_the_order_of_the_file() {
        let got = dropped_of("[machines]\nBad = \"x\"\nstudio = \"\"\npi = \"-x\"\n");
        assert_eq!(got.len(), 3);
        assert!(got[0].starts_with("machines.Bad:"), "{got:?}");
        assert!(got[1].starts_with("machines.studio:"), "{got:?}");
        assert!(got[2].starts_with("machines.pi:"), "{got:?}");
    }

    fn paths_with(config: Option<&str>) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&config_dir).unwrap();
        if let Some(text) = config {
            std::fs::write(config_dir.join("config.toml"), text).unwrap();
        }
        let paths = Paths::rooted_at(config_dir, state_dir);
        (dir, paths)
    }

    #[test]
    fn a_missing_file_or_table_loads_as_no_machines() {
        let (_dir, paths) = paths_with(None);
        assert_eq!(load_machines(&paths), Ok(vec![]));
        let (_dir, paths) = paths_with(Some("[ui]\ntheme = \"dark\"\n"));
        assert_eq!(load_machines(&paths), Ok(vec![]));
    }

    #[test]
    fn a_good_table_loads_in_order() {
        let (_dir, paths) = paths_with(Some(
            "[machines]\npi = \"pi\"\nstudio = \"studio.local\"\n[machines.attic]\nssh = \"a\"\ntaw = \"~/t\"\n",
        ));
        assert_eq!(
            load_machines(&paths),
            Ok(vec![
                machine("pi", "pi", None),
                machine("studio", "studio.local", None),
                machine("attic", "a", Some("~/t")),
            ])
        );
    }

    #[test]
    fn one_dropped_line_refuses_the_whole_table() {
        let (_dir, paths) =
            paths_with(Some("[machines]\nstudio = \"studio.local\"\nBad = \"x\"\n"));
        let file = paths.config_file.display().to_string();
        let file = match dirs::home_dir() {
            Some(home) => crate::session::naming::tilde(&paths.config_file, &home),
            None => file,
        };
        assert_eq!(
            load_machines(&paths),
            Err(MachinesDoubt::Dropped {
                file: file.clone(),
                line: "machines.Bad: a machine's name is lowercase letters, digits, - and _, \
                       starting with a letter, at most 32 characters"
                    .to_string()
            })
        );
        assert_eq!(
            load_machines(&paths).unwrap_err().to_string(),
            format!(
                "config {file} has a line in [machines] taw drops (machines.Bad: a machine's name \
                 is lowercase letters, digits, - and _, starting with a letter, at most 32 characters)"
            )
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_not_loaded() {
        let (_dir, paths) = paths_with(Some("[machines\nstudio = \"x\"\n"));
        match load_machines(&paths) {
            Err(MachinesDoubt::NotLoaded { why, .. }) => {
                assert!(!why.is_empty() && !why.contains('\n'), "{why:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_link_to_a_file_that_is_not_there_is_not_loaded() {
        let (dir, paths) = paths_with(None);
        std::os::unix::fs::symlink(dir.path().join("gone"), &paths.config_file).unwrap();
        assert!(matches!(
            load_machines(&paths),
            Err(MachinesDoubt::NotLoaded { .. })
        ));
    }

    #[test]
    fn a_change_of_the_file_is_read_once_it_has_stayed_the_same_for_a_look() {
        let at = |secs: u64, len: u64| {
            Stamp(Some(Seen {
                modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                len,
                inode: 7,
                changed: (secs as i64, 0),
            }))
        };
        let mut watch = ConfigWatch::new(at(1, 10));
        assert!(!watch.settled(at(1, 10)), "nothing changed");
        assert!(!watch.settled(at(2, 12)), "a change is not read at once");
        assert!(
            !watch.settled(at(3, 15)),
            "a file still being written waits"
        );
        assert!(watch.settled(at(3, 15)), "the same twice in a row is read");
        assert!(!watch.settled(at(3, 15)), "and only once");
        assert!(
            !watch.settled(Stamp(None)),
            "a file that is gone waits one look"
        );
        assert!(watch.settled(Stamp(None)), "and is read as no machines");
        assert!(
            !watch.settled(at(1, 10)),
            "a file back as it was first waits"
        );
        assert!(watch.settled(at(1, 10)), "and is read");
    }

    #[test]
    fn the_look_at_the_file_sees_a_change_of_its_length_and_its_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(tmp.path().join("config"), tmp.path().join("state"));
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        let none = stamp_of(&paths);
        assert_eq!(none, Stamp(None));
        std::fs::write(&paths.config_file, "a").unwrap();
        let one = stamp_of(&paths);
        assert_ne!(one, none);
        std::fs::write(&paths.config_file, "ab").unwrap();
        assert_ne!(stamp_of(&paths), one);
        std::fs::remove_file(&paths.config_file).unwrap();
        assert_eq!(stamp_of(&paths), none);
    }

    #[test]
    fn a_file_renamed_into_place_with_its_old_time_and_length_is_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(tmp.path().join("config"), tmp.path().join("state"));
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, "a = 1\n").unwrap();
        let before = stamp_of(&paths);
        let modified = std::fs::metadata(&paths.config_file)
            .unwrap()
            .modified()
            .unwrap();
        // Another file of the same length, given the old modified time, renamed over the config.
        let copy = tmp.path().join("config.toml.bak");
        std::fs::write(&copy, "b = 2\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        std::fs::rename(&copy, &paths.config_file).unwrap();
        assert_ne!(
            stamp_of(&paths),
            before,
            "a different file with the same modified time and length is noticed"
        );
    }

    #[test]
    fn a_read_made_while_the_file_was_changing_is_not_used_and_the_change_waits_to_settle_again() {
        let at = |secs: u64, len: u64| {
            Stamp(Some(Seen {
                modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                len,
                inode: 7,
                changed: (secs as i64, 0),
            }))
        };
        let mut watch = ConfigWatch::new(at(1, 10));
        assert!(!watch.settled(at(2, 12)));
        assert!(watch.settled(at(2, 12)), "settled: the file is read");
        assert!(
            !watch.read_was_of(at(2, 3)),
            "the file changed during the read: it is not used"
        );
        assert!(
            !watch.settled(at(2, 5)),
            "the file still changing waits: the change counts as not yet read"
        );
        assert!(
            watch.settled(at(2, 5)),
            "and is read once it stays the same"
        );
        assert!(
            watch.read_was_of(at(2, 5)),
            "a read the file did not change under is used"
        );
        assert!(!watch.settled(at(2, 5)), "and nothing is left to read");
    }
}
