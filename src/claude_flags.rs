//! The flags that set how a Claude session runs, which `taw session restore` types back after
//! `claude` so that a pane comes back as it was started (`--effort max`, `--model <name>`).
//!
//! Claude's `SessionStart` hook reports the session with `taw pane report-agent-session`, and the verb
//! reads the exact arguments of the pane's Claude process ([`kept`]) and keeps the listed flags in the
//! pane option [`OPTION`], in the tmux command that writes the report. A save copies them into the
//! snapshot (`PaneSnap::agent_claude_flags`), and a restore types them ([`crate::session::restore`]).
//!
//! Arguments can carry credentials, so only the flags of [`WITH_VALUE`] and [`ALONE`] are kept, each
//! with its value where it has one. Everything else is dropped unread and nothing is logged: not the
//! flags that choose a session (`--resume`, `-r`, `--continue`, `-c`, `--session-id`,
//! `--fork-session`), not print mode, not a prompt, not any other flag.

/// The pane option that holds the kept flags, as a JSON list of words ([`encode`]). It is part of an
/// agent's record: it goes when the agent leaves the pane or another takes it.
pub const OPTION: &str = "@agent_claude_flags";

/// The flags that take a value, as Claude's `--help` and its binary spell them. Each was found in the
/// strings of the installed binary before it was listed (never by running it).
pub const WITH_VALUE: [&str; 4] = ["--effort", "--model", "--permission-mode", "--agent"];

/// The flags that take none.
pub const ALONE: [&str; 1] = ["--dangerously-skip-permissions"];

/// The most words kept, and the most bytes in a value: a value past either is no model name or mode,
/// and is dropped with its flag. The option is written in one tmux command with the report.
const MAX_WORDS: usize = 16;
const MAX_VALUE_BYTES: usize = 128;

/// The flags of a Claude process worth keeping, from its exact arguments (`argv[0]` first), as the
/// words `--flag value` or `--flag` in the order given. `--flag=value` is read and written back as
/// `--flag value`. Reading stops at `--`. A flag with an empty value, one with no value after it, one
/// with a value that holds a control character or is too long, and every other argument, are dropped.
/// `None`, which is arguments that could not be read exactly (only the line `ps` prints, with its
/// boundaries gone), keeps nothing: a line is never guessed at.
pub fn kept(argv: Option<&[String]>) -> Vec<String> {
    let Some(argv) = argv else {
        return Vec::new();
    };
    let mut words: Vec<String> = Vec::new();
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if ALONE.contains(&arg.as_str()) {
            if words.len() < MAX_WORDS {
                words.push(arg.clone());
            }
            continue;
        }
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let Some(flag) = WITH_VALUE.iter().find(|name| **name == flag) else {
            continue;
        };
        // The next argument is the value as the program reads it, whatever it looks like, so it is
        // not read again as a flag of its own.
        let Some(value) = value.or_else(|| args.next().cloned()) else {
            continue;
        };
        if usable(&value) && words.len() + 2 <= MAX_WORDS {
            words.push((*flag).to_string());
            words.push(value);
        }
    }
    words
}

/// Whether `value` can be typed as a flag's value: not empty, not long, and no control character
/// (the line is typed into a shell, where one could end it).
fn usable(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_VALUE_BYTES && !value.chars().any(char::is_control)
}

/// The words as the option holds them: a JSON list, which keeps each word whole whatever it holds.
pub fn encode(words: &[String]) -> String {
    serde_json::to_string(words).unwrap_or_default()
}

/// The kept flags an option holds, read back through [`kept`] again so that a list that was edited by
/// hand or written by something else never types more than the listed flags. An unreadable or empty
/// value is no flags.
pub fn decode(text: &str) -> Vec<String> {
    let words: Vec<String> = serde_json::from_str(text).unwrap_or_default();
    refiltered(&words)
}

/// `words` (a saved list) as [`kept`] would keep them.
pub fn refiltered(words: &[String]) -> Vec<String> {
    let mut argv = vec![String::new()];
    argv.extend(words.iter().cloned());
    kept(Some(&argv))
}

/// The efforts `claude --help` lists for `--effort`, and so the only words read from a transcript.
const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// How much of the end of a transcript is read: the turn records are written as the session goes, so
/// the last one is near the end (within 160 KB on every transcript looked at), and a file of
/// hundreds of megabytes is never read whole.
const WINDOW: u64 = 2 * 1024 * 1024;

/// The effort the last turn of a session ran at, from the end of its transcript `bytes`, read from
/// the turn records' `"perTurnEffort":` field (Claude 2.1.308 writes it beside `"effort"`, and
/// Claude's own reader takes the per-turn value unless it is absent). Scanning from the end, an
/// occurrence of the key gives:
/// - a quoted level word after it: that level;
/// - `null` after it: the level of the `"effort":"<level>",` right before the key, a session whose
///   per-turn field is off carries its level only there (no such pair, or an unknown word in it, is
///   no result);
/// - any other value, a value cut off at the end of `bytes`, or a backslash before the key's opening
///   quote (text inside a JSON string, as a transcript quotes this very field): no result.
///
/// In every no-result case the occurrence before it is looked at, so the last record that gives a
/// level wins. Not the nested `"output_config":{"effort":...}` of the request blobs in the same
/// file, which is another thing and has no such key. Subagents write their own files in 2.1.308, so
/// a subagent's level is not in the main transcript, and the two levels of a record never differed
/// in the transcripts looked at. Read as bytes: the window can begin inside a multibyte character.
pub fn turn_effort(bytes: &[u8]) -> Option<&'static str> {
    const KEY: &[u8] = b"\"perTurnEffort\":";
    let mut end = bytes.len();
    while end >= KEY.len() {
        let at = bytes[..end].windows(KEY.len()).rposition(|w| w == KEY)?;
        end = at + KEY.len() - 1;
        if at > 0 && bytes[at - 1] == b'\\' {
            continue;
        }
        let rest = &bytes[at + KEY.len()..];
        if let Some(value) = rest.strip_prefix(b"\"") {
            let Some(close) = value.iter().take(8).position(|b| *b == b'"') else {
                continue;
            };
            if let Some(level) = level_named(&value[..close]) {
                return Some(level);
            }
        } else if rest.starts_with(b"null")
            && matches!(rest.get(4), Some(b',' | b'}'))
            && let Some(level) = level_before(&bytes[..at])
        {
            return Some(level);
        }
    }
    None
}

/// The level whose word is `word`.
fn level_named(word: &[u8]) -> Option<&'static str> {
    LEVELS
        .iter()
        .copied()
        .find(|level| level.as_bytes() == word)
}

/// The level of the `"effort":"<level>",` that ends `before`, the bytes ahead of a `perTurnEffort`
/// key: immediately before it, and not escaped.
fn level_before(before: &[u8]) -> Option<&'static str> {
    LEVELS.iter().copied().find(|level| {
        let pair = format!("\"effort\":\"{level}\",");
        before.ends_with(pair.as_bytes())
            && before
                .len()
                .checked_sub(pair.len() + 1)
                .is_none_or(|i| before[i] != b'\\')
    })
}

/// [`turn_effort`] of the last 2 MiB of the file at `path`. Opened only when `std::fs::metadata` says
/// it is a regular file (a FIFO, a device or a directory is never opened: a read of one could wait
/// for ever), and then by [`read_regular`], which does not trust that check. A missing or unreadable
/// file is `None`, silently. Nothing but the level word leaves.
pub fn read_turn_effort(path: &std::path::Path) -> Option<&'static str> {
    #[cfg(test)]
    READS.with(|reads| reads.set(reads.get() + 1));
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    turn_effort(&read_regular(path)?)
}

/// The last 2 MiB of the file at `path`, `None` when it cannot be opened or is not a regular file.
/// Opened without waiting (`O_NONBLOCK`: a path swapped for a FIFO after the check before it would
/// otherwise block the open for ever, with no writer), and the kind is checked again on the handle
/// before anything is read.
fn read_regular(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    file.seek(SeekFrom::Start(meta.len().saturating_sub(WINDOW)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(WINDOW).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

#[cfg(test)]
thread_local! {
    /// How many times [`read_turn_effort`] ran on this thread, which tests read to prove a plan reads
    /// a transcript once and a pane that does not use the flags not at all.
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The count of [`read_turn_effort`] calls on this thread so far.
#[cfg(test)]
pub fn reads_so_far() -> usize {
    READS.with(std::cell::Cell::get)
}

/// `flags` (kept flags, [`refiltered`]) with `--effort <level>` in place of the one they hold, or at
/// the end when they hold none; every other flag and the order stay. `None`, a transcript that gave no
/// level, leaves them as they are. A second `--effort` pair, which Claude would let win, goes, so the
/// level is typed once.
pub fn with_effort(flags: &[String], level: Option<&str>) -> Vec<String> {
    let Some(level) = level else {
        return flags.to_vec();
    };
    let mut words: Vec<String> = Vec::new();
    let mut found = false;
    let mut at = 0;
    while at < flags.len() {
        if ALONE.contains(&flags[at].as_str()) || at + 1 >= flags.len() {
            words.push(flags[at].clone());
            at += 1;
            continue;
        }
        if flags[at] == "--effort" {
            if !found {
                words.push(flags[at].clone());
                words.push(level.to_string());
            }
            found = true;
        } else {
            words.push(flags[at].clone());
            words.push(flags[at + 1].clone());
        }
        at += 2;
    }
    if !found {
        words.push("--effort".to_string());
        words.push(level.to_string());
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn kept_of(words: &[&str]) -> Vec<String> {
        kept(Some(&argv(words)))
    }

    #[test]
    fn the_listed_flags_are_kept_with_their_values_in_the_order_given() {
        assert_eq!(
            kept_of(&[
                "claude",
                "--model",
                "x",
                "--dangerously-skip-permissions",
                "--effort",
                "max",
                "--permission-mode",
                "plan",
                "--agent",
                "reviewer",
            ]),
            argv(&[
                "--model",
                "x",
                "--dangerously-skip-permissions",
                "--effort",
                "max",
                "--permission-mode",
                "plan",
                "--agent",
                "reviewer",
            ])
        );
    }

    #[test]
    fn the_equals_form_is_read_and_written_back_as_two_words() {
        assert_eq!(
            kept_of(&["claude", "--effort=max", "--model", "a=b", "--agent=x=y"]),
            argv(&["--effort", "max", "--model", "a=b", "--agent", "x=y"])
        );
    }

    #[test]
    fn a_node_or_script_start_is_read_past_its_program_and_script() {
        assert_eq!(
            kept_of(&[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "--effort",
                "high",
            ]),
            argv(&["--effort", "high"])
        );
        assert_eq!(
            kept_of(&["/bin/sh", "/opt/bin/claude", "--model", "opus"]),
            argv(&["--model", "opus"])
        );
    }

    #[test]
    fn reading_stops_at_a_double_dash() {
        assert_eq!(
            kept_of(&["claude", "--effort", "max", "--", "--model", "x"]),
            argv(&["--effort", "max"])
        );
        assert_eq!(
            kept_of(&["claude", "--", "--effort", "max"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_flags_that_choose_a_session_print_mode_and_a_prompt_are_dropped() {
        assert_eq!(
            kept_of(&[
                "claude",
                "--resume",
                "abc",
                "-r",
                "def",
                "--continue",
                "-c",
                "--session-id",
                "11111111-2222-3333-4444-555555555555",
                "--fork-session",
                "-p",
                "--print",
                "--output-format",
                "json",
                "--effort",
                "max",
                "a prompt that says --model x",
                "--add-dir",
                "/a",
                "/b",
            ]),
            argv(&["--effort", "max"])
        );
        assert_eq!(
            kept_of(&["claude", "--resume=abc", "--session-id=x"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_flag_with_an_empty_or_missing_value_is_dropped() {
        assert_eq!(
            kept_of(&["claude", "--effort", "", "--model=", "--agent", "x"]),
            argv(&["--agent", "x"])
        );
        assert_eq!(
            kept_of(&["claude", "--dangerously-skip-permissions", "--model"]),
            argv(&["--dangerously-skip-permissions"])
        );
        assert_eq!(
            kept_of(&["claude", "--permission-mode"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_value_is_the_next_argument_whatever_it_looks_like() {
        // As the program reads it: `--model` takes `--effort`, which is not a flag of its own here.
        assert_eq!(
            kept_of(&["claude", "--model", "--effort", "max"]),
            argv(&["--model", "--effort"])
        );
    }

    #[test]
    fn a_flag_that_is_only_alike_is_not_kept() {
        assert_eq!(
            kept_of(&[
                "claude",
                "--effort-level",
                "max",
                "--modelx",
                "y",
                "--dangerously-skip-permissions=1",
                "--Model",
                "z",
                "-model",
                "z",
            ]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_value_with_a_control_character_or_too_long_is_dropped_with_its_flag() {
        assert_eq!(
            kept_of(&[
                "claude", "--model", "a\nb", "--agent", "x\u{1b}y", "--effort", "max"
            ]),
            argv(&["--effort", "max"])
        );
        let long = "m".repeat(MAX_VALUE_BYTES + 1);
        assert_eq!(kept_of(&["claude", "--model", &long]), Vec::<String>::new());
        let fits = "m".repeat(MAX_VALUE_BYTES);
        assert_eq!(
            kept_of(&["claude", "--model", &fits]),
            argv(&["--model", &fits])
        );
    }

    #[test]
    fn what_is_kept_is_bounded_and_never_cut_between_a_flag_and_its_value() {
        let mut words = vec!["claude"];
        for _ in 0..MAX_WORDS {
            words.extend(["--model", "x"]);
        }
        let kept = kept_of(&words);
        assert_eq!(kept.len(), MAX_WORDS);
        assert_eq!(kept.last().map(String::as_str), Some("x"));
    }

    #[test]
    fn the_cap_is_sixteen_words_whatever_the_flags_are() {
        // A flag with no value is one word each: twenty of them keep sixteen.
        let mut words = vec!["claude"];
        words.extend(["--dangerously-skip-permissions"; 20]);
        assert_eq!(kept_of(&words).len(), 16);
        // A flag and its value that would end past the cap are dropped whole, not cut.
        let mut words = vec!["claude", "--dangerously-skip-permissions"];
        for _ in 0..8 {
            words.extend(["--model", "x"]);
        }
        let kept = kept_of(&words);
        assert_eq!(kept.len(), 15);
        assert_eq!(kept.last().map(String::as_str), Some("x"));
    }

    #[test]
    fn arguments_that_could_not_be_read_exactly_keep_nothing() {
        assert_eq!(kept(None), Vec::<String>::new());
    }

    #[test]
    fn the_option_holds_the_words_whole_and_reads_back_only_the_listed_flags() {
        let words = argv(&["--model", "a b'c", "--dangerously-skip-permissions"]);
        assert_eq!(decode(&encode(&words)), words);
        assert_eq!(decode(""), Vec::<String>::new());
        assert_eq!(decode("not json"), Vec::<String>::new());
        assert_eq!(
            decode(r#"["--resume","x","--model","m","--effort"]"#),
            argv(&["--model", "m"])
        );
        assert_eq!(
            refiltered(&argv(&["--effort", "max"])),
            argv(&["--effort", "max"])
        );
    }

    const RECORD: &str = r#"{"type":"assistant","uuid":"11111111-2222-4333-8444-555555555555","timestamp":"2026-10-01T07:45:17.220Z","advisorModel":"claude-opus-5-5","effort":"max","perTurnEffort":"LEVEL","session_id":"3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42","userType":"external","entrypoint":"cli","cwd":"/work/repo"}"#;

    fn record(level: &str) -> String {
        RECORD.replace("LEVEL", level)
    }

    #[test]
    fn the_last_turn_record_names_the_effort_and_all_five_levels_are_read() {
        for level in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(turn_effort(record(level).as_bytes()), Some(level));
        }
        let two = format!("{}\n{}\n", record("max"), record("high"));
        assert_eq!(turn_effort(two.as_bytes()), Some("high"));
        let three = format!(
            "{}\n{}\n{}\n",
            record("low"),
            record("max"),
            record("medium")
        );
        assert_eq!(turn_effort(three.as_bytes()), Some("medium"));
    }

    /// A turn record with `effort` and `per_turn` as raw JSON values (`"max"`, `null`).
    fn pair(effort: &str, per_turn: &str) -> String {
        RECORD
            .replace("\"effort\":\"max\"", &format!("\"effort\":{effort}"))
            .replace("\"LEVEL\"", per_turn)
    }

    #[test]
    fn a_null_per_turn_level_is_the_effort_right_before_it() {
        for level in ["low", "medium", "high", "xhigh", "max"] {
            let text = pair(&format!("\"{level}\""), "null");
            assert_eq!(turn_effort(text.as_bytes()), Some(level), "{text}");
        }
        // Neither a level before the key nor an unknown word there is a result.
        for effort in ["null", "\"ultra\"", "\"MAX\"", "\"\"", "5"] {
            let text = pair(effort, "null");
            assert_eq!(turn_effort(text.as_bytes()), None, "{text}");
        }
        // Not immediately before the key: another field between, or a space.
        let between = r#"{"effort":"max","x":1,"perTurnEffort":null}"#;
        assert_eq!(turn_effort(between.as_bytes()), None);
        let spaced = r#"{"effort":"max", "perTurnEffort":null}"#;
        assert_eq!(turn_effort(spaced.as_bytes()), None);
        // A `null` cut off at the end of the bytes is not a value.
        let cut = r#"{"effort":"max","perTurnEffort":nu"#;
        assert_eq!(turn_effort(cut.as_bytes()), None);
        let no_end = r#"{"effort":"max","perTurnEffort":null"#;
        assert_eq!(turn_effort(no_end.as_bytes()), None);
        // An escaped quote before the pair is text, not the field.
        let quoted = r#"{"t":"\"effort":"max","perTurnEffort":null}"#;
        assert_eq!(turn_effort(quoted.as_bytes()), None);
    }

    #[test]
    fn the_per_turn_level_wins_over_a_differing_effort_and_a_null_effort_is_no_bar() {
        let text = pair("\"low\"", "\"max\"");
        assert_eq!(turn_effort(text.as_bytes()), Some("max"));
        let text = pair("null", "\"high\"");
        assert_eq!(turn_effort(text.as_bytes()), Some("high"));
    }

    #[test]
    fn a_record_that_gives_no_level_is_skipped_for_the_one_before_it() {
        let both_null = pair("null", "null");
        let text = format!("{}\n{both_null}\n", pair("\"low\"", "\"medium\""));
        assert_eq!(turn_effort(text.as_bytes()), Some("medium"));
        // Any other value of the field is skipped the same way.
        for other in ["5", "true", "{}", "[]", "\"\"", "\"ultra\""] {
            let text = format!("{}\n{}\n", record("xhigh"), pair("\"low\"", other));
            assert_eq!(turn_effort(text.as_bytes()), Some("xhigh"), "{other}");
        }
        // A null record after earlier ones gives its own level, not an earlier record's.
        let text = format!("{}\n{}\n", record("max"), pair("\"high\"", "null"));
        assert_eq!(turn_effort(text.as_bytes()), Some("high"));
        let text = format!(
            "{}\n{}\n{}\n",
            record("max"),
            pair("\"high\"", "null"),
            pair("null", "null")
        );
        assert_eq!(turn_effort(text.as_bytes()), Some("high"));
    }

    #[test]
    fn the_nested_output_config_effort_is_not_a_turn_record() {
        let blob = r#"{"type":"api-request-blob","max_tokens":1,"output_config":{"effort":"low"}}"#;
        assert_eq!(turn_effort(blob.as_bytes()), None);
        // A record's own `effort` is not read either: only the field of the turn.
        let only = r#"{"type":"assistant","effort":"max"}"#;
        assert_eq!(turn_effort(only.as_bytes()), None);
        // The blob after the record does not hide it.
        let both = format!("{}\n{blob}\n", record("high"));
        assert_eq!(turn_effort(both.as_bytes()), Some("high"));
    }

    #[test]
    fn text_inside_a_json_string_is_no_match_and_the_record_before_it_still_is() {
        // A message that quotes the field: a backslash before the opening quote.
        let quoted = r#"{"text":"say \"perTurnEffort":"max" here"}"#;
        assert_eq!(turn_effort(quoted.as_bytes()), None);
        let after = format!("{}\n{quoted}\n", record("low"));
        assert_eq!(turn_effort(after.as_bytes()), Some("low"));
    }

    #[test]
    fn an_unknown_word_or_a_value_cut_off_is_no_match() {
        assert_eq!(turn_effort(record("ultra").as_bytes()), None);
        assert_eq!(turn_effort(record("").as_bytes()), None);
        assert_eq!(turn_effort(record("MAX").as_bytes()), None);
        let cut = r#"{"type":"assistant","perTurnEffort":"ma"#;
        assert_eq!(turn_effort(cut.as_bytes()), None);
        let cut_whole_word = r#"{"type":"assistant","perTurnEffort":"max"#;
        assert_eq!(turn_effort(cut_whole_word.as_bytes()), None);
        assert_eq!(turn_effort(b""), None);
        assert_eq!(turn_effort(b"\"perTurnEffort\":\""), None);
    }

    fn file_in(dir: &tempfile::TempDir, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_missing_file_a_directory_and_a_fifo_give_nothing_and_are_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_turn_effort(&dir.path().join("none.jsonl")), None);
        assert_eq!(read_turn_effort(dir.path()), None);
        let fifo = dir.path().join("pipe.jsonl");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.is_ok_and(|status| status.success()), "mkfifo");
        // A guard thread: a regression that opens the FIFO would wait for ever for a writer.
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_turn_effort(&path));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(got) => assert_eq!(got, None),
            Err(_) => {
                // Release the stuck reader so the run can end, then fail.
                let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
                panic!("read_turn_effort waited on a FIFO");
            }
        }
    }

    #[test]
    fn the_open_itself_does_not_wait_on_a_fifo_with_no_writer() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.jsonl");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.is_ok_and(|status| status.success()), "mkfifo");
        // `read_regular` is the open and the check on the handle, with no check of the path before
        // it: the path that was a file when it was looked at and is a FIFO when it is opened.
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_regular(&path));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(got) => assert_eq!(got, None),
            Err(_) => {
                // Release the stuck open so the run can end, then fail.
                let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
                panic!("read_regular waited on a FIFO");
            }
        }
        // A regular file is read.
        assert_eq!(
            read_regular(&file_in(&dir, b"abc")).as_deref(),
            Some(&b"abc"[..])
        );
    }

    #[test]
    fn only_the_last_two_mebibytes_of_a_file_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let filler = vec![b'x'; WINDOW as usize];
        // The field is before the window: not found.
        let mut early = record("max").into_bytes();
        early.extend_from_slice(&filler);
        assert_eq!(read_turn_effort(&file_in(&dir, &early)), None);
        // Inside it: found.
        let mut inside = filler.clone();
        inside.extend_from_slice(record("high").as_bytes());
        inside.extend_from_slice(&filler[..WINDOW as usize - 4096]);
        assert_eq!(read_turn_effort(&file_in(&dir, &inside)), Some("high"));
        // A small file is read whole.
        assert_eq!(
            read_turn_effort(&file_in(&dir, record("low").as_bytes())),
            Some("low")
        );
    }

    #[test]
    fn a_window_that_starts_inside_a_multibyte_character_is_read() {
        let dir = tempfile::tempdir().unwrap();
        // Three-byte characters, so that the cut lands inside one whatever the offset.
        let mut bytes = "\u{20ac}".repeat(WINDOW as usize / 3 + 10).into_bytes();
        bytes.extend_from_slice(record("medium").as_bytes());
        bytes.extend_from_slice("\u{20ac}".repeat(100).as_bytes());
        if (bytes.len() as u64 - WINDOW).is_multiple_of(3) {
            bytes.push(b'a');
        }
        let start = bytes.len() as u64 - WINDOW;
        assert!(
            std::str::from_utf8(&bytes[start as usize..]).is_err(),
            "cuts a character"
        );
        assert_eq!(read_turn_effort(&file_in(&dir, &bytes)), Some("medium"));
    }

    #[test]
    fn a_level_replaces_the_saved_effort_in_place_or_is_appended() {
        let flags = argv(&[
            "--model",
            "m",
            "--effort",
            "low",
            "--dangerously-skip-permissions",
        ]);
        assert_eq!(
            with_effort(&flags, Some("max")),
            argv(&[
                "--model",
                "m",
                "--effort",
                "max",
                "--dangerously-skip-permissions"
            ])
        );
        let none = argv(&["--dangerously-skip-permissions", "--model", "m"]);
        assert_eq!(
            with_effort(&none, Some("high")),
            argv(&[
                "--dangerously-skip-permissions",
                "--model",
                "m",
                "--effort",
                "high"
            ])
        );
        assert_eq!(with_effort(&[], Some("low")), argv(&["--effort", "low"]));
        // Typed once, whatever was saved.
        let twice = argv(&["--effort", "low", "--model", "m", "--effort", "high"]);
        assert_eq!(
            with_effort(&twice, Some("max")),
            argv(&["--effort", "max", "--model", "m"])
        );
    }

    #[test]
    fn no_level_leaves_the_flags_alone() {
        let flags = argv(&["--effort", "low", "--model", "m"]);
        assert_eq!(with_effort(&flags, None), flags);
        assert_eq!(with_effort(&[], None), Vec::<String>::new());
    }
}
