//! What the Codex install does to `config.toml`: Codex keeps its own `[features]` table there,
//! and a second one makes Codex refuse to start, so taw puts `hooks = true` into the user's
//! table and never writes a config that does not parse. Every install here runs under a scratch
//! `$HOME` and `$CODEX_HOME`; nothing is read from or written to the real ones.
//!
//! Every test holds `ENV_LOCK`: they set process-wide variables.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use taw::integration::{Incomplete, IntegrationError, install, status, uninstall};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: the caller holds `ENV_LOCK` for the guard's whole lifetime.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// A scratch `$HOME` with a Codex folder in it.
struct Machine {
    _root: tempfile::TempDir,
    codex: PathBuf,
    _env: Vec<EnvGuard>,
}

impl Machine {
    fn new() -> Machine {
        let root = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let codex = base.join("codex");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&codex).unwrap();
        let env = vec![
            EnvGuard::set("HOME", &home),
            EnvGuard::set("CODEX_HOME", &codex),
        ];
        Machine {
            _root: root,
            codex,
            _env: env,
        }
    }

    fn config(&self) -> PathBuf {
        self.codex.join("config.toml")
    }

    fn script(&self) -> PathBuf {
        self.codex.join("taw-agent-state.sh")
    }

    fn pristine(&self) -> PathBuf {
        self.codex.join("taw-agent-state.sh.orig.toml")
    }

    fn write_config(&self, text: &str) {
        fs::write(self.config(), text).unwrap();
    }

    fn config_text(&self) -> String {
        fs::read_to_string(self.config()).unwrap()
    }

    fn install(&self) -> anyhow::Result<taw::integration::Report> {
        install("codex", &self.codex.join("taw-placeholder"))
    }

    /// Nothing of taw's is on disk: the hook script, the hooks file and the pristine copy.
    fn assert_nothing_installed(&self) {
        assert!(!self.script().exists(), "the hook script was written");
        assert!(
            !self.codex.join("hooks.json").exists(),
            "hooks.json was written"
        );
        assert!(!self.pristine().exists(), "a pristine copy was written");
    }
}

const OLD_BLOCK: &str =
    "# >>> taw integration >>>\n[features]\nhooks = true\n# <<< taw integration <<<\n";
const TAWS_LINE: &str = "hooks = true  # taw integration";
const MARKED_HEADER: &str = "[features]  # taw integration";
/// What an install writes into a file with no `features`: marked lines, no delimited block.
const NEW_TABLE: &str = "[features]  # taw integration\nhooks = true  # taw integration\n";
const THEIR_TABLE: &str = "[features]\ncodex_hooks = true\ngoals = true\n";

fn parsed(text: &str) -> toml::Table {
    text.parse::<toml::Table>()
        .unwrap_or_else(|err| panic!("{}: {text}", err.message()))
}

fn features_of(text: &str) -> toml::Table {
    parsed(text)["features"].as_table().unwrap().clone()
}

fn refusal(result: anyhow::Result<taw::integration::Report>) -> String {
    let err = result.expect_err("the install must refuse");
    assert!(
        matches!(
            err.downcast_ref::<IntegrationError>(),
            Some(IntegrationError::UnusableConfig { .. })
        ),
        "{err:#}"
    );
    format!("{err:#}")
}

#[test]
fn an_install_over_the_old_installs_broken_file_repairs_it_and_an_uninstall_leaves_their_table() {
    let _g = lock();
    let m = Machine::new();
    // The file that broke: their table near the end, two blank lines, taw's old block.
    let theirs = format!("model = \"gpt-5\"\n\n[tui]\ntheme = \"dark\"\n\n{THEIR_TABLE}");
    m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}"));
    assert!(
        m.config_text().parse::<toml::Table>().is_err(),
        "the fixture must be the broken shape"
    );

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(installed.matches("[features]").count(), 1, "{installed}");
    assert!(!installed.contains("taw integration >>>"), "{installed}");
    let features = features_of(&installed);
    assert_eq!(features.len(), 3, "{installed}");
    assert_eq!(features["codex_hooks"].as_bool(), Some(true));
    assert_eq!(features["goals"].as_bool(), Some(true));
    assert_eq!(features["hooks"].as_bool(), Some(true));
    assert!(
        installed.contains(&format!("[features]\n{TAWS_LINE}\ncodex_hooks = true\n")),
        "the line goes right under the header: {installed}"
    );

    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), theirs);
}

#[test]
fn a_repaired_file_whose_pristine_copy_is_their_own_file_reverts_to_it_byte_for_byte() {
    let _g = lock();
    let m = Machine::new();
    // What an earlier install saved: their file before taw touched it, table included.
    let theirs = format!("model = \"gpt-5\"\n\n{THEIR_TABLE}");
    fs::write(m.pristine(), &theirs).unwrap();
    m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}"));

    m.install().expect("install");
    assert_eq!(features_of(&m.config_text()).len(), 3);

    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), theirs);
    assert!(!m.pristine().exists());
}

#[test]
fn a_features_table_without_the_old_block_gets_the_line_under_its_header() {
    let _g = lock();
    let m = Machine::new();
    for header in [
        "[features]",
        "[features]   # what Codex keeps here",
        "  [ features ]\t",
    ] {
        let theirs =
            format!("model = \"x\"\n\n{header}\ngoals = true\n\n[tui]\ntheme = \"dark\"\n");
        m.write_config(&theirs);

        m.install().expect("install");

        let installed = m.config_text();
        assert!(
            installed.contains(&format!("{header}\n{TAWS_LINE}\ngoals = true\n")),
            "{header:?}: {installed}"
        );
        let features = features_of(&installed);
        assert_eq!(features["hooks"].as_bool(), Some(true));
        assert_eq!(features["goals"].as_bool(), Some(true));
        assert_eq!(parsed(&installed)["tui"]["theme"].as_str(), Some("dark"));
        // Installing again changes nothing.
        m.install().expect("second install");
        assert_eq!(m.config_text(), installed, "{header:?}");

        uninstall("codex").expect("uninstall");
        assert_eq!(m.config_text(), theirs, "{header:?}");
    }
}

#[test]
fn a_header_that_ends_the_file_without_a_newline_still_gets_its_line() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("[features]");

    m.install().expect("install");

    assert_eq!(m.config_text(), format!("[features]\n{TAWS_LINE}\n"));
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), "[features]");
}

#[test]
fn a_file_with_windows_line_endings_gets_a_line_with_the_same_ending() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("[features]\r\ngoals = true\r\n");

    m.install().expect("install");

    assert_eq!(
        m.config_text(),
        format!("[features]\r\n{TAWS_LINE}\r\ngoals = true\r\n")
    );
}

#[test]
fn a_table_that_already_sets_hooks_true_gets_nothing_added() {
    let _g = lock();
    let m = Machine::new();
    for line in [
        "hooks = true",
        "hooks=true",
        "  hooks   =   true   # mine",
        "\"hooks\" = true",
    ] {
        let theirs = format!("[features]\ngoals = true\n{line}\n");
        m.write_config(&theirs);

        m.install().expect("install");

        assert_eq!(m.config_text(), theirs, "{line:?}");
        uninstall("codex").expect("uninstall");
        assert_eq!(m.config_text(), theirs, "{line:?}");
    }
}

#[test]
fn the_old_block_beside_a_table_that_sets_hooks_true_is_taken_out_and_nothing_is_added() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "[features]\nhooks = true\ngoals = true\n";
    m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}"));

    m.install().expect("install");

    assert_eq!(m.config_text(), theirs);
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), theirs);
}

#[test]
fn a_table_that_sets_hooks_false_is_refused_with_its_line_and_nothing_changes() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "model = \"x\"\n\n[features]\ngoals = true\nhooks = false  # off for now\n";
    m.write_config(theirs);

    let message = refusal(m.install());

    assert!(message.contains("config.toml"), "{message}");
    assert!(message.contains("line 5"), "{message}");
    assert!(message.contains("hooks = false"), "{message}");
    assert!(message.contains("hooks = true"), "{message}");
    assert_eq!(m.config_text(), theirs);
    m.assert_nothing_installed();
}

#[test]
fn a_features_table_written_another_way_is_refused_and_nothing_changes() {
    let _g = lock();
    let m = Machine::new();
    for theirs in [
        "features.goals = true\n\n[tui]\ntheme = \"dark\"\n",
        "features = { goals = true }\n",
        "[\"features\"]\ngoals = true\n",
        "[features.nested]\ngoals = true\n",
        "[ 'features' ]\ngoals = true\n",
    ] {
        m.write_config(theirs);

        let message = refusal(m.install());

        assert!(message.contains("config.toml"), "{theirs}: {message}");
        assert!(message.contains("hooks = true"), "{theirs}: {message}");
        assert!(message.contains("by hand"), "{theirs}: {message}");
        assert_eq!(m.config_text(), theirs);
        m.assert_nothing_installed();
    }
}

#[test]
fn a_header_that_is_only_text_inside_a_string_is_never_edited() {
    let _g = lock();
    let m = Machine::new();
    // Their features are dotted keys, and a multi-line string holds a line that looks like the
    // header. A line put under that "header" would change their string, not their table.
    let theirs = "features.goals = true\nnote = \"\"\"\n[features]\n\"\"\"\n";
    m.write_config(theirs);

    let message = refusal(m.install());

    assert!(message.contains("hooks = true"), "{message}");
    assert_eq!(m.config_text(), theirs);
    m.assert_nothing_installed();
}

#[test]
fn dotted_keys_that_set_hooks_true_need_nothing_added() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "features.goals = true\nfeatures.hooks = true\n";
    m.write_config(theirs);

    m.install().expect("install");

    assert_eq!(m.config_text(), theirs);
}

#[test]
fn a_file_without_a_features_table_gets_two_marked_lines_and_no_delimited_block() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "model = \"x\"\n\n[tui]\ntheme = \"dark\"\n";
    m.write_config(theirs);

    m.install().expect("install");

    assert_eq!(m.config_text(), format!("{theirs}\n{NEW_TABLE}"));
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), theirs);
}

#[test]
fn no_config_at_all_gets_the_marked_lines_and_an_uninstall_removes_the_file() {
    let _g = lock();
    let m = Machine::new();

    m.install().expect("install");

    assert_eq!(m.config_text(), NEW_TABLE);
    uninstall("codex").expect("uninstall");
    assert!(!m.config().exists());
}

#[test]
fn a_file_that_does_not_parse_is_left_alone_with_a_message() {
    let _g = lock();
    let m = Machine::new();
    for theirs in [
        "model = \n[features]\ngoals = true\n",
        "[features]\ngoals = true\n[features]\nother = 1\n",
        // Their own mistake, and taw's old block as well: the block comes out of the reading,
        // not out of the file, and the file is still broken without it.
        "[tui\ntheme = 1\n\n# >>> taw integration >>>\n[features]\nhooks = true\n# <<< taw integration <<<\n",
    ] {
        m.write_config(theirs);

        let message = refusal(m.install());

        assert!(message.contains("config.toml"), "{theirs}: {message}");
        assert!(message.contains("does not parse"), "{theirs}: {message}");
        assert!(message.contains("line "), "{theirs}: {message}");
        assert_eq!(m.config_text(), theirs);
        m.assert_nothing_installed();
    }
}

#[test]
fn the_refusal_does_not_print_the_file() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("token = \"sk-secret-value\"\nmodel = \n");

    let message = refusal(m.install());

    assert!(!message.contains("sk-secret-value"), "{message}");
}

#[test]
fn an_uninstall_takes_out_only_the_line_taw_added() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("[features]\ngoals = true\n");
    m.install().expect("install");
    // They edit the file after the install, so the pristine copy no longer matches.
    let edited = m
        .config_text()
        .replace("goals = true", "goals = true\nextra = 1");
    m.write_config(&edited);

    uninstall("codex").expect("uninstall");

    assert_eq!(m.config_text(), "[features]\ngoals = true\nextra = 1\n");
}

fn note_of(m: &Machine) -> (Option<Incomplete>, bool) {
    let status = status("codex").expect("status");
    assert!(status.installed, "{:?}", m.codex);
    (status.incomplete.clone(), status.outdated())
}

#[test]
fn status_calls_a_config_codex_cannot_load_incomplete_and_a_reinstall_repairs_it() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    assert_eq!(note_of(&m), (None, false));

    // What the old install left: their table and, after it, taw's block.
    m.write_config(&format!("{THEIR_TABLE}\n\n{OLD_BLOCK}"));

    let (incomplete, outdated) = note_of(&m);
    let Some(Incomplete::Reinstall(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(outdated);
    assert!(note.contains("cannot load"), "{note}");
    assert!(note.contains("taw integration install codex"), "{note}");

    m.install().expect("reinstall repairs it");
    assert_eq!(note_of(&m), (None, false));
}

#[test]
fn status_says_when_the_file_is_broken_for_another_reason_and_a_reinstall_would_not_help() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    m.write_config("model = \n");

    let (incomplete, outdated) = note_of(&m);

    let Some(Incomplete::Waiting(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(!outdated);
    assert!(note.contains("cannot load"), "{note}");
    assert!(note.contains("config.toml"), "{note}");
}

#[test]
fn status_reports_a_table_that_sets_hooks_false() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    m.write_config("[features]\nhooks = false\n");

    let (incomplete, outdated) = note_of(&m);

    let Some(Incomplete::Waiting(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(!outdated, "a reinstall would refuse");
    assert!(note.contains("hooks = false"), "{note}");
    assert!(note.contains("hooks = true"), "{note}");
}

#[test]
fn status_is_current_for_every_good_shape() {
    let _g = lock();
    let m = Machine::new();
    for theirs in [
        "[features]\ngoals = true\n",
        "[features]\nhooks = true\n",
        "features.hooks = true\n",
        "model = \"x\"\n",
    ] {
        m.write_config(theirs);
        m.install().expect("install");
        assert_eq!(note_of(&m), (None, false), "{theirs}");
    }
}

#[test]
fn status_says_when_the_config_holds_no_hooks_true_at_all() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    // Somebody took taw's block out by hand.
    m.write_config("model = \"x\"\n");

    let (incomplete, outdated) = note_of(&m);

    let Some(Incomplete::Reinstall(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(outdated);
    assert!(note.contains("hooks = true"), "{note}");
}

// ---------------------------------------------------------------------------------------
// Taw's own old block always goes, only marked lines are written, and the
// refusals say where.
// ---------------------------------------------------------------------------------------

/// A refusal that also repaired the file: the message says what was removed and why the install
/// stops.
fn repaired_refusal(result: anyhow::Result<taw::integration::Report>) -> String {
    let message = refusal(result);
    assert!(message.contains("took its older block out"), "{message}");
    assert!(message.contains("keeping every key of yours"), "{message}");
    message
}

#[test]
fn taws_old_block_comes_out_even_when_the_install_then_refuses() {
    let _g = lock();
    let m = Machine::new();
    // Each: their file, the words the refusal must hold, and the line it names.
    for (theirs, shape, line) in [
        ("[features]\nhooks = false\n", "hooks = false", "line 2"),
        (
            "model = \"x\"\nfeatures.goals = true\n",
            "dotted keys",
            "line 2",
        ),
        ("features = { goals = true }\n", "inline table", "line 1"),
    ] {
        let _ = fs::remove_file(m.config());
        m.install().expect("a clean install first");
        let broken = format!("{theirs}\n\n{OLD_BLOCK}");
        m.write_config(&broken);
        assert!(
            broken.parse::<toml::Table>().is_err(),
            "the fixture must be a file Codex cannot load: {theirs}"
        );

        // Before the repair: out of date, and the note says Codex cannot load the file.
        let (incomplete, outdated) = note_of(&m);
        let Some(Incomplete::Reinstall(note)) = incomplete else {
            panic!("{theirs}: {incomplete:?}");
        };
        assert!(outdated, "{theirs}");
        assert!(
            note.contains("Codex cannot load config.toml: an older taw's block is in it; a reinstall repairs it"),
            "{note}"
        );

        let message = repaired_refusal(m.install());

        assert!(message.contains(shape), "{theirs}: {message}");
        assert!(message.contains(line), "{theirs}: {message}");
        // Every byte of theirs is as it was, the block is gone, and Codex can load the file.
        assert_eq!(m.config_text(), theirs);
        parsed(&m.config_text());

        // After: the refusal's own note, nothing to reinstall.
        let (incomplete, outdated) = note_of(&m);
        assert!(
            matches!(incomplete, Some(Incomplete::Waiting(_))),
            "{incomplete:?}"
        );
        assert!(!outdated, "{theirs}");
        uninstall("codex").expect("uninstall");
    }
}

#[test]
fn a_refusal_that_repaired_the_file_writes_no_script_and_no_hooks_file() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "[features]\nhooks = false\n";
    m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}"));

    repaired_refusal(m.install());

    assert_eq!(m.config_text(), theirs);
    m.assert_nothing_installed();
}

#[test]
fn a_refusal_without_taws_block_does_not_claim_a_repair() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("[features]\nhooks = false\n");

    let message = refusal(m.install());

    assert!(!message.contains("older block"), "{message}");
}

#[test]
fn a_file_with_no_features_gets_two_marked_lines_that_codex_can_add_to() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "model = \"x\"\n";
    m.write_config(theirs);

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(
        installed,
        format!("{theirs}\n{MARKED_HEADER}\n{TAWS_LINE}\n")
    );
    assert!(!installed.contains(">>>"), "{installed}");

    // Codex saves its own switch into the table it finds, under taw's line.
    let saved = installed.replace(TAWS_LINE, &format!("{TAWS_LINE}\ngoals = true"));
    m.write_config(&saved);
    m.install().expect("reinstall");
    assert_eq!(m.config_text(), saved, "a reinstall keeps their key");
    assert_eq!(note_of(&m), (None, false));

    uninstall("codex").expect("uninstall");
    let after = m.config_text();
    assert_eq!(features_of(&after).len(), 1, "{after}");
    assert_eq!(features_of(&after)["goals"].as_bool(), Some(true));
    assert!(after.contains("[features]"), "the header stays: {after}");
    assert!(!after.contains(TAWS_LINE), "{after}");
}

#[test]
fn nothing_added_gives_the_file_back_byte_for_byte() {
    let _g = lock();
    let m = Machine::new();
    for theirs in ["model = \"x\"\n", "model = \"x\"", ""] {
        m.write_config(theirs);
        m.install().expect("install");
        uninstall("codex").expect("uninstall");
        if theirs.is_empty() {
            assert!(!m.config().exists());
        } else {
            assert_eq!(m.config_text(), theirs);
        }
    }
}

#[test]
fn an_uninstall_takes_the_marked_table_out_when_it_is_empty_and_something_else_changed() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("model = \"x\"\n\n[tui]\ntheme = \"dark\"\n");
    m.install().expect("install");
    // The pristine copy no longer matches, so the strip does the work.
    let edited = m.config_text().replace("\"x\"", "\"y\"");
    m.write_config(&edited);

    uninstall("codex").expect("uninstall");

    assert_eq!(
        m.config_text(),
        "model = \"y\"\n\n[tui]\ntheme = \"dark\"\n"
    );

    // Marked table in the middle of the file.
    m.write_config(&format!(
        "a = 1\n\n{MARKED_HEADER}\n{TAWS_LINE}\n\n[tui]\nb = 2\n"
    ));
    m.install().expect("reinstall");
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), "a = 1\n\n[tui]\nb = 2\n");
}

#[test]
fn an_unmarked_header_is_the_users_and_stays_with_nothing_but_their_table() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("[features]\ngoals = true\n");
    m.install().expect("install");
    m.write_config(&m.config_text().replace("goals = true", "goals = false"));

    uninstall("codex").expect("uninstall");

    assert_eq!(m.config_text(), "[features]\ngoals = false\n");

    // Their header with nothing in it stays too.
    m.write_config("[features]\n");
    m.install().expect("install");
    m.write_config(&format!("a = 1\n{}", m.config_text()));
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), "a = 1\n[features]\n");
}

#[test]
fn an_old_block_with_a_key_of_theirs_inside_becomes_the_table_alone_in_the_file() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n# <<< taw integration <<<\n";
    let theirs = "model = \"x\"\n";
    m.write_config(&format!("{theirs}\n\n{block}"));

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(
        installed,
        format!("{theirs}\n\n{MARKED_HEADER}\n{TAWS_LINE}\ngoals = true\n")
    );
    assert!(!installed.contains(">>>"), "{installed}");
    assert_eq!(features_of(&installed).len(), 2);

    uninstall("codex").expect("uninstall");
    let after = m.config_text();
    assert_eq!(
        features_of(&after)["goals"].as_bool(),
        Some(true),
        "{after}"
    );
    assert!(!after.contains("hooks"), "{after}");

    // An uninstall straight from the old block keeps the key too, under an unmarked header.
    m.write_config(&format!("{theirs}\n\n{block}"));
    uninstall("codex").expect("uninstall");
    assert_eq!(
        m.config_text(),
        format!("{theirs}\n\n[features]\ngoals = true\n")
    );
}

#[test]
fn an_old_block_with_a_key_of_theirs_inside_moves_it_under_their_own_table() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n# <<< taw integration <<<\n";
    let theirs = "[features]\ncodex_hooks = true\n";
    m.write_config(&format!("{theirs}\n\n{block}"));

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(
        installed,
        format!("[features]\n{TAWS_LINE}\ngoals = true\ncodex_hooks = true\n")
    );
    assert_eq!(features_of(&installed).len(), 3);

    // Straight from the old block, no reinstall: the key moves, taw's line is not added.
    m.write_config(&format!("{theirs}\n\n{block}"));
    uninstall("codex").expect("uninstall");
    assert_eq!(
        m.config_text(),
        "[features]\ngoals = true\ncodex_hooks = true\n"
    );
}

#[test]
fn a_key_set_both_in_the_old_block_and_in_their_table_is_refused_with_both_lines() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\ngoals = false\n# <<< taw integration <<<\n";
    let broken = format!("[features]\nother = 1\ngoals = true\n\n\n{block}");
    m.write_config(&broken);

    let message = refusal(m.install());

    assert!(message.contains("goals"), "{message}");
    assert!(message.contains("line 3"), "their line: {message}");
    assert!(message.contains("line 9"), "the block's line: {message}");
    assert_eq!(m.config_text(), broken);
    m.assert_nothing_installed();

    // An uninstall says so too, before it deletes anything.
    let err = uninstall("codex").expect_err("the uninstall must say what clashes");
    assert!(format!("{err:#}").contains("goals"), "{err:#}");
    assert_eq!(m.config_text(), broken);
}

#[test]
fn two_old_blocks_become_one_table_and_an_uninstall_leaves_their_own() {
    let _g = lock();
    let m = Machine::new();
    let theirs = format!("model = \"gpt-5\"\n\n{THEIR_TABLE}");
    m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}\n\n{OLD_BLOCK}"));

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(installed.matches("[features]").count(), 1, "{installed}");
    assert!(!installed.contains(">>>"), "{installed}");
    assert_eq!(features_of(&installed).len(), 3);
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), theirs);

    // Two blocks and no table of theirs at all.
    m.write_config(&format!("model = \"x\"\n\n{OLD_BLOCK}\n\n{OLD_BLOCK}"));
    m.install().expect("install");
    assert_eq!(
        m.config_text(),
        format!("model = \"x\"\n\n{MARKED_HEADER}\n{TAWS_LINE}\n")
    );
}

#[test]
fn a_header_that_is_only_text_in_a_string_before_the_real_table_is_skipped() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "note = \"\"\"\n[features]\n\"\"\"\n\n[features]\ngoals = true\n";
    m.write_config(theirs);

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(
        installed,
        "note = \"\"\"\n[features]\n\"\"\"\n\n[features]\nhooks = true  # taw integration\ngoals = true\n"
    );
    assert_eq!(parsed(&installed)["note"].as_str(), Some("[features]\n"));
    assert_eq!(features_of(&installed).len(), 2);
}

#[test]
fn every_by_hand_refusal_names_the_shape_and_the_line() {
    let _g = lock();
    let m = Machine::new();
    for (theirs, shape, line, add) in [
        (
            "model = \"x\"\n\nfeatures.goals = true\n",
            "dotted keys",
            "line 3",
            "features.hooks = true",
        ),
        (
            "model = \"x\"\n\nfeatures = { goals = true }\n",
            "inline table",
            "line 3",
            "inside its braces",
        ),
        (
            "model = \"x\"\n\n[\"features\"]\ngoals = true\n",
            "quoted",
            "line 3",
            "under that header",
        ),
        (
            "model = \"x\"\n\n[features.nested]\ngoals = true\n",
            "[features.<name>] sub-table",
            "line 3",
            "add a [features] table",
        ),
        (
            "model = \"x\"\n\n[[features]]\ngoals = true\n",
            "array of tables",
            "line 3",
            "change it to a [features] table",
        ),
        (
            "model = \"x\"\nfeatures = 3\n",
            "not a table",
            "line 2",
            "change it to a [features] table",
        ),
    ] {
        m.write_config(theirs);

        let message = refusal(m.install());

        assert!(message.contains(shape), "{theirs}: {message}");
        assert!(message.contains(line), "{theirs}: {message}");
        assert!(message.contains(add), "{theirs}: {message}");
        assert!(message.contains("by hand"), "{theirs}: {message}");
        assert_eq!(m.config_text(), theirs);
    }
}

#[test]
fn a_by_hand_refusal_never_prints_the_line() {
    let _g = lock();
    let m = Machine::new();
    m.write_config("features.token = \"sk-secret-value\"\n");

    let message = refusal(m.install());

    assert!(!message.contains("sk-secret-value"), "{message}");
}

#[test]
fn an_uninstall_takes_taws_block_out_of_a_file_that_is_broken_for_another_reason() {
    let _g = lock();
    let m = Machine::new();
    let theirs = "[tui\ntheme = 1\n";
    m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}"));

    uninstall("codex").expect("uninstall");

    assert_eq!(m.config_text(), theirs);
}

#[test]
fn status_names_the_conflict_when_a_key_is_set_in_the_block_and_in_their_table() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    m.write_config(
        "[features]\ngoals = true\n\n\n# >>> taw integration >>>\n[features]\nhooks = true\ngoals = false\n# <<< taw integration <<<\n",
    );

    let (incomplete, outdated) = note_of(&m);

    let Some(Incomplete::Waiting(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(!outdated, "a reinstall would refuse");
    assert!(note.contains("cannot load"), "{note}");
    assert!(note.contains("goals"), "{note}");
}

#[test]
fn status_calls_a_lone_old_block_out_of_date_and_a_reinstall_swaps_it_for_marked_lines() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    m.write_config(&format!("model = \"x\"\n\n\n{OLD_BLOCK}"));

    let (incomplete, outdated) = note_of(&m);

    let Some(Incomplete::Reinstall(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(outdated);
    assert!(note.contains("older taw"), "{note}");
    assert!(!note.contains("cannot load"), "the file loads fine: {note}");
    m.install().expect("reinstall");
    assert_eq!(note_of(&m), (None, false));
}

// ---------------------------------------------------------------------------------------
// An old block that Codex saved tables into is converted in place, the notes
// say "cannot load" only for a file that does not parse, and the refusals give their own words.
// ---------------------------------------------------------------------------------------

/// The shape Codex leaves: its tables saved before the end marker of taw's old block, so they
/// sit inside it. The file parses and Codex runs the hook.
fn block_with_tables(table: &str) -> String {
    format!(
        "model = \"gpt-5\"\n\n# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n\n{table}\n# <<< taw integration <<<\n"
    )
}

const SAVED_TABLES: [&str; 3] = [
    "[projects.\"/Users/x/dev\"]\ntrust_level = \"trusted\"",
    "[mcp_servers.docs]\ncommand = \"x\"",
    "[notice]\nhide_full_access_warning = true",
];

#[test]
fn an_old_block_with_tables_codex_saved_inside_it_is_converted_in_place() {
    let _g = lock();
    let m = Machine::new();
    for table in SAVED_TABLES {
        let original = block_with_tables(table);
        let before = parsed(&original);
        assert_eq!(features_of(&original).len(), 2, "the fixture loads fine");
        let _ = fs::remove_file(m.config());
        m.install().expect("a clean install first");
        m.write_config(&original);

        // Status: out of date, and it does not claim Codex cannot load the file.
        let (incomplete, outdated) = note_of(&m);
        let Some(Incomplete::Reinstall(note)) = incomplete else {
            panic!("{table}: {incomplete:?}");
        };
        assert!(outdated, "{table}");
        assert!(note.contains("older taw"), "{note}");
        assert!(!note.contains("cannot load"), "{note}");

        m.install().expect("install exits 0");

        let installed = m.config_text();
        assert!(!installed.contains(">>>"), "{installed}");
        assert!(!installed.contains("<<<"), "{installed}");
        assert_eq!(
            installed,
            format!("model = \"gpt-5\"\n\n{MARKED_HEADER}\n{TAWS_LINE}\ngoals = true\n\n{table}\n"),
            "every other line stays where it is"
        );
        // Everything the user had is there, and so is the hook.
        assert_eq!(parsed(&installed), before, "{installed}");
        assert_eq!(features_of(&installed)["hooks"].as_bool(), Some(true));
        assert_eq!(note_of(&m), (None, false), "{table}");

        uninstall("codex").expect("uninstall exits 0");

        let after = m.config_text();
        let after_table = parsed(&after);
        assert!(!features_of(&after).contains_key("hooks"), "{after}");
        assert_eq!(features_of(&after)["goals"].as_bool(), Some(true));
        let mut expected = before.clone();
        expected["features"].as_table_mut().unwrap().remove("hooks");
        assert_eq!(after_table, expected, "{after}");
        assert!(!after.contains(TAWS_LINE), "{after}");
        let _ = fs::remove_file(m.config());
    }
}

#[test]
fn an_uninstall_straight_from_an_old_block_with_saved_tables_leaves_all_of_theirs() {
    let _g = lock();
    let m = Machine::new();
    for table in SAVED_TABLES {
        let original = block_with_tables(table);
        m.write_config(&original);

        uninstall("codex").expect("uninstall exits 0");

        assert_eq!(
            m.config_text(),
            format!("model = \"gpt-5\"\n\n[features]\ngoals = true\n\n{table}\n")
        );
    }
}

#[test]
fn an_old_block_holding_only_a_table_codex_saved_and_no_key_is_converted_too() {
    let _g = lock();
    let m = Machine::new();
    let original = "# >>> taw integration >>>\n[features]\nhooks = true\n\n[notice]\nx = 1\n# <<< taw integration <<<\n";
    m.write_config(original);

    m.install().expect("install");

    assert_eq!(
        m.config_text(),
        format!("{MARKED_HEADER}\n{TAWS_LINE}\n\n[notice]\nx = 1\n")
    );
    uninstall("codex").expect("uninstall");
    assert_eq!(
        parsed(&m.config_text())["notice"]["x"].as_integer(),
        Some(1)
    );
}

#[test]
fn the_notes_say_cannot_load_only_for_a_file_that_does_not_parse() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("install");
    // What Codex writes when the user switches hooks off in the table the block gave it.
    let original = "model = \"x\"\n\n# >>> taw integration >>>\n[features]\nhooks = false\n# <<< taw integration <<<\n";
    m.write_config(original);
    assert!(original.parse::<toml::Table>().is_ok());

    let (incomplete, _) = note_of(&m);
    let Some(Incomplete::Reinstall(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(!note.contains("cannot load"), "{note}");
    assert!(note.contains("older taw"), "{note}");

    let message = refusal(m.install());

    assert!(message.contains("took its older block out"), "{message}");
    assert!(!message.contains("cannot load"), "{message}");
    assert!(!message.contains("from loading"), "{message}");
    assert!(message.contains("hooks = false"), "{message}");
    parsed(&m.config_text());
}

#[test]
fn a_file_codex_could_not_load_still_gets_the_cannot_load_words() {
    let _g = lock();
    let m = Machine::new();
    m.write_config(&format!("[features]\nhooks = false\n\n\n{OLD_BLOCK}"));

    let message = repaired_refusal(m.install());

    assert!(message.contains("from loading the file"), "{message}");
}

/// A broken `features` of the user's (dotted, inline) beside the old block, which holds no key of
/// theirs: the block comes out and the install stops with the refusal's own note.
#[test]
fn after_a_repair_status_gives_the_refusals_own_note_for_dotted_and_inline_features() {
    let _g = lock();
    let m = Machine::new();
    for (theirs, shape, line, add) in [
        (
            "model = \"x\"\nfeatures.goals = true\n",
            "dotted keys",
            "line 2",
            "features.hooks = true next to them",
        ),
        (
            "features = { goals = true }\n",
            "inline table",
            "line 1",
            "hooks = true inside its braces",
        ),
    ] {
        let _ = fs::remove_file(m.config());
        m.install().expect("a clean install first");
        m.write_config(&format!("{theirs}\n\n{OLD_BLOCK}"));
        repaired_refusal(m.install());
        assert_eq!(m.config_text(), theirs);

        let (incomplete, outdated) = note_of(&m);

        let Some(Incomplete::Waiting(note)) = incomplete else {
            panic!("{theirs}: {incomplete:?}");
        };
        assert!(!outdated, "{theirs}");
        assert!(note.contains(shape), "{note}");
        assert!(note.contains(line), "{note}");
        assert!(note.contains(add), "{note}");
        assert!(
            !note.contains("[features] table"),
            "never advise a [features] table here: {note}"
        );
        uninstall("codex").expect("uninstall");
    }
}

#[test]
fn dotted_or_inline_features_beside_an_old_block_with_keys_of_theirs_names_the_blocks_lines() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n# <<< taw integration <<<\n";
    for (theirs, shape, theirs_line) in [
        (
            "model = \"x\"\nfeatures.codex_hooks = true\n",
            "dotted keys",
            "line 2",
        ),
        (
            "model = \"x\"\nfeatures = { codex_hooks = true }\n",
            "inline table",
            "line 2",
        ),
    ] {
        let broken = format!("{theirs}\n\n{block}");
        let _ = fs::remove_file(m.config());
        m.install().expect("a clean install first");
        m.write_config(&broken);

        // Status: Codex cannot load the file, and a reinstall alone does not change that.
        let (incomplete, outdated) = note_of(&m);
        let Some(Incomplete::Waiting(note)) = incomplete else {
            panic!("{theirs}: {incomplete:?}");
        };
        assert!(!outdated, "{theirs}");
        assert!(note.contains("cannot load"), "{note}");
        assert!(note.contains("line 8"), "{note}");

        let message = refusal(m.install());

        // The block's key sits on line 8: model, features, two blanks, marker, header, hooks, goals.
        assert!(message.contains(shape), "{message}");
        assert!(message.contains(theirs_line), "{message}");
        assert!(message.contains("line 8"), "the block's key: {message}");
        assert!(message.contains("cannot load"), "{message}");
        assert!(message.contains("both define features"), "{message}");
        assert!(message.contains("delete the block"), "{message}");
        assert!(message.contains("install again"), "{message}");
        assert!(
            !message.contains("goals"),
            "no key or value printed: {message}"
        );
        assert_eq!(m.config_text(), broken, "nothing is changed");

        // An uninstall says it in its own words.
        let err = uninstall("codex").expect_err("the uninstall says what to do");
        let text = format!("{err:#}");
        assert!(text.contains("line 8"), "{text}");
        assert!(text.contains("uninstall again"), "{text}");
        assert!(!text.contains("taw does not install"), "{text}");
        assert_eq!(m.config_text(), broken);
    }
}

#[test]
fn an_uninstall_that_cannot_convert_takes_out_at_least_the_markers_and_taws_line() {
    let _g = lock();
    let m = Machine::new();
    // A second block that holds a table of its own: the conversion cannot move it under one
    // header, and an install refuses. An uninstall does not fail with that text.
    let second = "# >>> taw integration >>>\n[features]\nhooks = true\n\n[notice]\nx = 1\n# <<< taw integration <<<\n";
    m.write_config(&format!("model = \"x\"\n\n{OLD_BLOCK}\n\n{second}"));
    let message = refusal(m.install());
    assert!(message.contains("by hand"), "{message}");

    uninstall("codex").expect("the uninstall does not refuse");

    let after = m.config_text();
    assert!(!after.contains(">>>"), "{after}");
    assert!(!after.contains("<<<"), "{after}");
    assert!(!after.contains("hooks"), "{after}");
    assert!(after.contains("[notice]"), "{after}");
}

#[test]
fn an_install_puts_one_blank_line_before_the_marked_header() {
    let _g = lock();
    let m = Machine::new();
    for (theirs, installed) in [
        ("model = \"x\"\n", format!("model = \"x\"\n\n{NEW_TABLE}")),
        ("model = \"x\"", format!("model = \"x\"\n\n{NEW_TABLE}")),
        (
            "model = \"x\"\n\n",
            format!("model = \"x\"\n\n\n{NEW_TABLE}"),
        ),
    ] {
        m.write_config(theirs);

        m.install().expect("install");

        assert_eq!(m.config_text(), installed, "{theirs:?}");
        uninstall("codex").expect("uninstall");
        assert_eq!(m.config_text(), theirs, "byte for byte: {theirs:?}");
    }
    // The file is not given back by the saved copy when something else changed: the blank line
    // that set the table off goes with it.
    m.write_config("model = \"x\"\n");
    m.install().expect("install");
    let edited = m.config_text().replace("model = \"x\"", "model = \"y\"");
    m.write_config(&edited);
    uninstall("codex").expect("uninstall");
    assert_eq!(m.config_text(), "model = \"y\"\n");
}

#[test]
fn a_hooks_line_in_another_table_of_the_old_block_is_not_taws() {
    let _g = lock();
    let m = Machine::new();
    // Taw's own line is gone (they took it out); a table Codex saved holds `hooks = true`.
    let original = "# >>> taw integration >>>\n[features]\ngoals = true\n\n[mcp_servers.x]\nhooks = true\n# <<< taw integration <<<\n";
    m.write_config(original);

    m.install().expect("install");

    let installed = m.config_text();
    assert_eq!(
        installed,
        format!("{MARKED_HEADER}\n{TAWS_LINE}\ngoals = true\n\n[mcp_servers.x]\nhooks = true\n")
    );
    assert_eq!(
        parsed(&installed)["mcp_servers"]["x"]["hooks"].as_bool(),
        Some(true)
    );
}

// ---------------------------------------------------------------------------------------
// Tables Codex saved into an old block stay where they are beside the user's own
// table too, comments in a block are not keys, a block's header goes when nothing is left under
// it, and a repair keeps the file's final newline.
// ---------------------------------------------------------------------------------------

const SAVED_PROJECT: &str = "[projects.\"/home/you/code/api\"]\ntrust_level = \"trusted\"\n";

/// An old block that holds a key of the user's and then a table Codex saved into it.
fn block_with_key_and_table() -> String {
    format!(
        "# >>> taw integration >>>\n[features]\nhooks = true\ncodex_hooks = true\n\n{SAVED_PROJECT}# <<< taw integration <<<\n"
    )
}

#[test]
fn a_table_codex_saved_into_an_old_block_stays_in_place_beside_their_own_features_table() {
    let _g = lock();
    let m = Machine::new();
    let original = format!(
        "[features]\ngoals = true\n\n\n{}",
        block_with_key_and_table()
    );
    assert!(original.parse::<toml::Table>().is_err(), "two features");
    m.install().expect("a clean install first");
    m.write_config(&original);

    // Before: Codex cannot load the file, and a reinstall repairs it.
    let (incomplete, outdated) = note_of(&m);
    let Some(Incomplete::Reinstall(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(outdated);
    assert!(note.contains("cannot load"), "{note}");

    m.install().expect("install exits 0");

    let installed = m.config_text();
    assert_eq!(
        installed,
        format!("[features]\n{TAWS_LINE}\ncodex_hooks = true\ngoals = true\n\n\n{SAVED_PROJECT}"),
        "the key moves under their header, the table stays where it was"
    );
    let table = parsed(&installed);
    assert_eq!(table["features"].as_table().unwrap().len(), 3);
    assert_eq!(
        table["projects"]["/home/you/code/api"]["trust_level"].as_str(),
        Some("trusted")
    );
    assert_eq!(note_of(&m), (None, false));

    uninstall("codex").expect("uninstall exits 0");

    let after = m.config_text();
    assert_eq!(
        after,
        format!("[features]\ncodex_hooks = true\ngoals = true\n\n\n{SAVED_PROJECT}")
    );
    assert!(parsed(&after)["projects"].is_table());

    // An uninstall straight from the old block does the same conversion.
    m.write_config(&original);
    uninstall("codex").expect("uninstall exits 0");
    assert_eq!(m.config_text(), after);
}

#[test]
fn a_saved_table_in_an_old_block_joins_a_table_of_the_same_family_the_file_already_has() {
    let _g = lock();
    let m = Machine::new();
    let original = format!(
        "[features]\ngoals = true\n\n[projects.\"/other\"]\ntrust_level = \"untrusted\"\n\n\n{}",
        block_with_key_and_table()
    );
    m.write_config(&original);

    m.install().expect("install");

    let installed = m.config_text();
    let table = parsed(&installed);
    assert_eq!(
        table["projects"].as_table().unwrap().len(),
        2,
        "{installed}"
    );
    assert_eq!(
        table["features"].as_table().unwrap().len(),
        3,
        "{installed}"
    );
    assert!(installed.contains(SAVED_PROJECT), "{installed}");
    assert!(!installed.contains(">>>"), "{installed}");
}

/// The residual shape the in-place move cannot place: a line of theirs after the end marker of a
/// block that holds a table, which would change tables. It is said in the block-keys words, and an
/// uninstall stops with the same text instead of leaving a file Codex still cannot load.
#[test]
fn a_block_that_cannot_be_placed_beside_their_table_says_cannot_load_and_an_uninstall_stops() {
    let _g = lock();
    let m = Machine::new();
    let original = format!(
        "[features]\ngoals = true\n\n\n{}model = \"x\"\n",
        block_with_key_and_table()
    );
    m.write_config(&original);

    let message = refusal(m.install());

    assert!(
        message.contains("line 1 is your [features] table"),
        "{message}"
    );
    assert!(message.contains("line 8"), "the block's key: {message}");
    assert!(
        message.contains("Codex cannot load the file while both define features"),
        "{message}"
    );
    assert!(message.contains("install again"), "{message}");
    assert_eq!(m.config_text(), original);

    let err = uninstall("codex").expect_err("the uninstall stops too");
    let text = format!("{err:#}");
    assert!(text.contains("uninstall again"), "{text}");
    assert_eq!(m.config_text(), original);
}

#[test]
fn a_saved_table_in_an_old_block_beside_their_table_with_a_key_in_both_is_refused() {
    let _g = lock();
    let m = Machine::new();
    let original = format!(
        "[features]\ncodex_hooks = false\n\n\n{}",
        block_with_key_and_table()
    );
    m.write_config(&original);

    let message = refusal(m.install());

    assert!(message.contains("codex_hooks"), "{message}");
    assert!(message.contains("line 2"), "their line: {message}");
    assert!(message.contains("line 8"), "the block's line: {message}");
    assert_eq!(m.config_text(), original);
}

#[test]
fn comments_in_an_old_block_are_not_keys_of_theirs_beside_dotted_features() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\n# a note\n# <<< taw integration <<<\n";
    let original = format!("features.goals = true\n\n\n{block}");
    m.install().expect("a clean install first");
    m.write_config(&original);

    let message = repaired_refusal(m.install());

    assert!(message.contains("dotted keys"), "{message}");
    assert!(
        message.contains("features.hooks = true next to them"),
        "{message}"
    );
    assert!(!message.contains("holds keys of yours"), "{message}");
    let repaired = m.config_text();
    assert_eq!(repaired, "features.goals = true\n\n\n# a note\n");
    parsed(&repaired);

    // Status gives the same note: the shape, the line, where the line goes.
    let (incomplete, _) = note_of(&m);
    let Some(Incomplete::Waiting(note)) = incomplete else {
        panic!("{incomplete:?}");
    };
    assert!(note.contains("dotted keys"), "{note}");
    assert!(!note.contains("holds keys of yours"), "{note}");

    // An uninstall never refuses for it.
    m.write_config(&original);
    uninstall("codex").expect("an uninstall exits 0");
    assert_eq!(m.config_text(), "features.goals = true\n\n\n# a note\n");
}

#[test]
fn a_comment_beside_inline_features_stays_and_does_not_make_the_block_hold_keys() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\n\n# mine\n\n# <<< taw integration <<<\n";
    m.write_config(&format!("features = {{ goals = true }}\n\n\n{block}"));

    let message = repaired_refusal(m.install());

    assert!(message.contains("inline table"), "{message}");
    assert!(!message.contains("holds keys of yours"), "{message}");
    assert!(m.config_text().contains("# mine"), "{}", m.config_text());
    parsed(&m.config_text());
}

#[test]
fn an_uninstall_straight_from_an_old_block_with_only_a_saved_table_leaves_no_features_header() {
    let _g = lock();
    let m = Machine::new();
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\n\n[notice]\nx = 1\n# <<< taw integration <<<\n";
    m.write_config(&format!("model = \"x\"\n\n\n{block}"));

    uninstall("codex").expect("uninstall");

    let after = m.config_text();
    assert_eq!(after, "model = \"x\"\n\n\n[notice]\nx = 1\n");
    assert!(!after.contains("[features]"), "{after}");

    // A comment under the header stays where it is; the header still goes.
    let with_note = "# >>> taw integration >>>\n[features]\nhooks = true\n# a note\n\n[notice]\nx = 1\n# <<< taw integration <<<\n";
    m.write_config(&format!("model = \"x\"\n\n\n{with_note}"));
    uninstall("codex").expect("uninstall");
    assert_eq!(
        m.config_text(),
        "model = \"x\"\n\n\n# a note\n\n[notice]\nx = 1\n"
    );

    // A key of theirs under the header keeps it.
    let with_key = "# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n\n[notice]\nx = 1\n# <<< taw integration <<<\n";
    m.write_config(&format!("model = \"x\"\n\n\n{with_key}"));
    uninstall("codex").expect("uninstall");
    assert_eq!(
        m.config_text(),
        "model = \"x\"\n\n\n[features]\ngoals = true\n\n[notice]\nx = 1\n"
    );
}

#[test]
fn every_repair_keeps_the_final_newline_a_file_had() {
    let _g = lock();
    let m = Machine::new();
    // One blank line before the block, which the old strip took together with the newline after
    // the end marker.
    let block = "# >>> taw integration >>>\n[features]\nhooks = true\n# <<< taw integration <<<\n";
    let with_key = "# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n# <<< taw integration <<<\n";
    for original in [
        // beside, with a table
        format!("[features]\ngoals = true\n\n{}", block_with_key_and_table()),
        // beside, keys only
        format!("[features]\ncodex_hooks = true\n\n{with_key}"),
        // dotted and inline, which are refused after the repair
        format!("features.goals = true\n\n{block}"),
        format!("features = {{ goals = true }}\n\n{block}"),
        // hooks = false in their table
        format!("[features]\nhooks = false\n\n{block}"),
        // a lone block
        format!("model = \"x\"\n\n{block}"),
    ] {
        m.write_config(&original);
        let _ = m.install();
        let installed = m.config_text();
        assert!(
            installed.ends_with('\n'),
            "install: {original:?} -> {installed:?}"
        );
        assert!(!installed.contains(">>>"), "{installed}");

        m.write_config(&original);
        let _ = uninstall("codex");
        let after = m.config_text();
        assert!(
            after.ends_with('\n'),
            "uninstall: {original:?} -> {after:?}"
        );
        assert!(!after.contains(">>>"), "{after}");
        let _ = fs::remove_file(m.pristine());
    }
    // A file that did not end in a newline does not get one.
    m.write_config(&format!("features.goals = true\n\n{}", block.trim_end()));
    let _ = m.install();
    assert!(!m.config_text().ends_with('\n'), "{:?}", m.config_text());
}

/// An older taw appended its block, with one blank line, to a file with no final newline, and
/// the user then pasted a table right after the end marker.
const BLOCK_THEN_TABLE: [&str; 2] = [
    "[mcp_servers.x]\ncommand = \"y\"\n",
    "[projects.\"/home/you/code/api\"]\ntrust_level = \"trusted\"\n",
];

#[test]
fn a_table_right_after_an_old_blocks_end_marker_is_never_joined_to_the_line_before_it() {
    let _g = lock();
    let m = Machine::new();
    for table in BLOCK_THEN_TABLE {
        let original = format!(
            "model = \"gpt-5\"\n\n# >>> taw integration >>>\n[features]\nhooks = true\ngoals = true\n\
             # <<< taw integration <<<\n{table}"
        );
        let before = parsed(&original);
        let _ = fs::remove_file(m.config());
        m.install().expect("a clean install first");

        // Status: out of date, and Codex can load the file, so it does not say it cannot.
        m.write_config(&original);
        let (incomplete, outdated) = note_of(&m);
        let Some(Incomplete::Reinstall(note)) = incomplete else {
            panic!("{table}: {incomplete:?}");
        };
        assert!(outdated, "{table}");
        assert!(!note.contains("cannot load"), "{note}");

        m.install().expect("install exits 0");
        let installed = m.config_text();
        assert!(!installed.contains(">>>"), "{installed}");
        assert_eq!(parsed(&installed)["model"].as_str(), Some("gpt-5"));
        assert_eq!(features_of(&installed)["hooks"].as_bool(), Some(true));
        assert_eq!(features_of(&installed)["goals"].as_bool(), Some(true));
        for key in before.keys().filter(|key| *key != "features") {
            assert_eq!(parsed(&installed)[key], before[key], "{installed}");
        }
        assert_eq!(note_of(&m), (None, false), "{table}");

        // An uninstall from the older block itself, as well as after the install.
        uninstall("codex").expect("uninstall exits 0");
        let after = m.config_text();
        assert!(!features_of(&after).contains_key("hooks"), "{after}");
        assert_eq!(features_of(&after)["goals"].as_bool(), Some(true));
        for key in before.keys().filter(|key| *key != "features") {
            assert_eq!(parsed(&after)[key], before[key], "{after}");
        }

        m.write_config(&original);
        uninstall("codex").expect("uninstall straight from the block exits 0");
        let after = m.config_text();
        assert!(!after.contains(">>>"), "{after}");
        assert_eq!(parsed(&after)["model"].as_str(), Some("gpt-5"));
        assert_eq!(features_of(&after)["goals"].as_bool(), Some(true));
        for key in before.keys().filter(|key| *key != "features") {
            assert_eq!(parsed(&after)[key], before[key], "{after}");
        }
        let _ = fs::remove_file(m.config());
    }
}

#[test]
fn a_file_that_loads_is_never_called_unloadable_because_taking_a_block_out_broke_it() {
    let _g = lock();
    let m = Machine::new();
    m.install().expect("a clean install first");
    // Both markers inside one string of theirs: the file loads, and cutting the "block" out
    // of it would not.
    let original = "note = \"# >>> taw integration >>> # <<< taw integration <<<\"\n";
    m.write_config(original);
    let status = status("codex").expect("status");
    if let Some(Incomplete::Waiting(note) | Incomplete::Reinstall(note)) = &status.incomplete {
        assert!(!note.contains("cannot load"), "{note}");
        assert!(!note.contains("does not parse"), "{note}");
    }
    let message = match m.install() {
        Ok(_) => String::new(),
        Err(err) => format!("{err:#}"),
    };
    assert!(!message.contains("does not parse"), "{message}");
    assert_eq!(
        parsed(&m.config_text())["note"].as_str(),
        Some(&original[8..original.len() - 2])
    );
}
