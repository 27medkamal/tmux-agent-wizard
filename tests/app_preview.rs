//! The preview of a pane on a machine, in `taw app`, end to end: the built binary against private tmux
//! servers, the far side's own `taw` behind the stand-in for ssh (no test runs a real ssh).
//!
//! Every wait is positive, with a 20 s deadline, and panics with what was seen.

mod common;

use std::time::Duration;

use common::machine::{FarSide, StandIn};
use common::owned::{Owned, own, record_names};
use common::{Server, ServerOptions, taw_bin, wait_for};

/// The terminal, this Mac, studio and the stand-in.
struct Rig {
    term: Server,
    mac: Server,
    far: FarSide,
    stand_in: StandIn,
}

/// A terminal as a person has one: a private server of its own whose pane is the terminal. Wide, so that a toast
/// is not cut.
fn terminal() -> Server {
    Server::start_with(ServerOptions {
        width: 200,
        height: 30,
        ..ServerOptions::default()
    })
}

/// This Mac with `studio` in `[machines]`, and studio: a session `garden` with the windows `main` and `tools`.
fn rig() -> Rig {
    let mac = Server::start();
    // Every pane is a row (the depth `all`), so that a pane can be previewed.
    mac.write_config("[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n");
    let far = FarSide::with_session("studio", "garden");
    far.server
        .tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    far.server
        .tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "tools"]);
    let stand_in = StandIn::new(&[&far]);
    Rig {
        term: terminal(),
        mac,
        far,
        stand_in,
    }
}

// ---------------------------------------------------------------- reading and waiting

fn terminal_screen(term: &Server) -> String {
    term.tmux(&["capture-pane", "-p", "-t", &term.pane()])
        .unwrap_or_default()
}

/// What a failure says besides the screen: the stand-in's calls and the apps of this build that run.
fn seen(rig: &Rig) -> String {
    let ps = std::process::Command::new("/bin/ps")
        .args(["-Ao", "pid=,ppid=,args="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default();
    let apps: Vec<&str> = ps
        .lines()
        .filter(|line| line.contains(&format!("{} app", taw_bin())))
        .collect();
    format!(
        "the terminal:\n{}\nthe last words of the stand-in's calls: {:#?}\nthe app's standard error: {:?}\nthe apps that run: {apps:#?}",
        terminal_screen(&rig.term)
            .lines()
            .filter(|line| !line
                .trim_matches(|c: char| c == ' ' || c == '\u{2502}')
                .is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        rig.stand_in
            .calls()
            .into_iter()
            .rev()
            .take(4)
            .map(|call| {
                let line = call.last().cloned().unwrap_or_default();
                line.split(" taw ").last().unwrap_or_default().to_string()
            })
            .collect::<Vec<_>>(),
        std::fs::read_to_string(rig.mac.work_dir().join("err")).unwrap_or_default()
    )
}

/// Waits (20 s) until `ready` holds, and fails with `what` and everything [`seen`] when it does not.
fn expect(rig: &Rig, what: &str, mut ready: impl FnMut() -> bool) {
    assert!(
        wait_for(&mut ready, Duration::from_secs(20)),
        "{what} never happened.\n{}",
        seen(rig)
    );
}

/// Waits (20 s) until `ready` holds of the terminal's screen.
fn expect_screen(rig: &Rig, what: &str, ready: impl Fn(&str) -> bool) {
    expect(rig, what, || ready(&terminal_screen(&rig.term)));
}

/// Types `keys` (tmux key names) into the terminal.
fn press(term: &Server, keys: &[&str]) {
    let pane = term.pane();
    let mut args = vec!["send-keys", "-t", pane.as_str()];
    args.extend_from_slice(keys);
    term.tmux_ok(&args);
}

/// The row of the list the cursor is on, as plain text: the line of the screen drawn with the selection's
/// background (the theme's own, fixed by the default), or empty.
fn selected_row(term: &Server) -> String {
    let styled = term
        .tmux(&["capture-pane", "-p", "-e", "-t", &term.pane()])
        .unwrap_or_default();
    for line in styled.lines() {
        if line.contains("48;2;69;71;90m") {
            let mut plain = String::new();
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for end in chars.by_ref() {
                        if end.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    plain.push(c);
                }
            }
            return plain;
        }
    }
    String::new()
}

/// Puts the cursor on the row that shows `needle`: `2` is studio's session, then `j` a row at a time, each step
/// waited for.
fn move_to(rig: &Rig, needle: &str) {
    move_to_in(rig, "2", "garden", needle);
}

/// The same from the session that key `key` selects, whose row says `session`.
fn move_to_in(rig: &Rig, key: &str, session: &str, needle: &str) {
    press(&rig.term, &[key]);
    expect(rig, "the cursor on studio's session", || {
        selected_row(&rig.term).contains(session)
    });
    for _ in 0..12 {
        let row = selected_row(&rig.term);
        if row.contains(needle) {
            return;
        }
        press(&rig.term, &["j"]);
        expect(rig, "the cursor moving down a row", || {
            selected_row(&rig.term) != row
        });
    }
    panic!(
        "no row with {needle} was reached; the cursor is on {:?}\n{}",
        selected_row(&rig.term),
        seen(rig)
    );
}

// ---------------------------------------------------------------- the app

/// The records of the apps that run, by file name, under this Mac's state folder.
fn records(mac: &Server) -> Vec<String> {
    record_names(
        std::fs::read_dir(mac.state_dir().join("apps"))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Types the app into the terminal as run for this Mac, with the stand-in as its ssh, and waits for its record (the
/// guard holds the pid from then on) and for studio's rows. Returns the app's pid.
fn start_app(rig: &Rig) -> u32 {
    let bin = rig.stand_in.bin.display().to_string();
    let env: String = rig
        .mac
        .taw_env()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .chain([("TAW_SSH_BIN", bin.as_str())])
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let line = format!(
        "env -u TMUX -u TMUX_PANE {env}'{}' app --child 2>'{}'; rc=$?; stty -a >'{}' 2>&1; echo $rc >'{}'",
        taw_bin(),
        rig.mac.work_dir().join("err").display(),
        rig.mac.work_dir().join("modes").display(),
        rig.mac.work_dir().join("status").display()
    );
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", &line]);
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "Enter"]);
    // The record first and the guard at once: a failed wait for the rows must not leave the app outside it.
    expect(rig, "one record of an app", || records(&rig.mac).len() == 1);
    let name = records(&rig.mac).remove(0);
    let pid: u32 = name.trim_end_matches(".json").parse().expect("a pid");
    own(pid);
    expect_screen(rig, "studio's rows on the list", |screen| {
        screen.contains(" taw app") && screen.contains("0:main") && screen.contains("1:tools")
    });
    pid
}

// ---------------------------------------------------------------- what the tests read

/// The terminal's screen with its escapes (`-e`): the colours as drawn.
fn styled_screen(term: &Server) -> String {
    term.tmux(&["capture-pane", "-p", "-e", "-t", &term.pane()])
        .unwrap_or_default()
}

/// Whether some line of `styled` draws `word` in red: tmux hands the outer terminal's pane red as the indexed
/// colour `38;5;1`, which is how the preview's `Color::Red` is drawn. Only the style escapes right before the
/// word are read (the red can come with its weight and its background, the terminal's own, as the preview draws
/// a pane on it), and the typed line the far shell echoes has no escape before its words at all.
fn word_in_red(styled: &str, word: &str) -> bool {
    styled.lines().any(|line| {
        let Some(at) = line.find(word) else {
            return false;
        };
        // The style escapes that end what comes before the word, the last first, up to anything else.
        let mut before = &line[..at];
        while let Some(start) = before.rfind("\u{1b}[") {
            let escape = &before[start..];
            let params = &escape[2..escape.len().max(3) - 1];
            if !escape.ends_with('m') || params.contains(|c: char| !c.is_ascii_digit() && c != ';')
            {
                return false;
            }
            if escape == "\u{1b}[38;5;1m" {
                return true;
            }
            before = &before[..start];
        }
        false
    })
}

/// Types `line` into the pane `garden:main` of studio and runs it.
fn far_runs(rig: &Rig, line: &str) {
    rig.far
        .server
        .tmux_ok(&["send-keys", "-t", "garden:main", "-l", line]);
    rig.far
        .server
        .tmux_ok(&["send-keys", "-t", "garden:main", "Enter"]);
}

/// The calls of the stand-in that read a pane's screen: the far command lines.
fn reads(stand_in: &StandIn) -> Vec<String> {
    stand_in
        .calls()
        .into_iter()
        .filter_map(|call| call.last().cloned())
        .filter(|line| line.contains(" taw 'pane' 'read'"))
        .collect()
}

/// The bound of a read of a pane (`READ_BOUND` in the app, in `src/tui/app_screen/remote_preview.rs`, which a test
/// outside the crate cannot name), in seconds. The unit test `the_read_bound_is_the_one_the_preview_hang_test_names`
/// there pins the app's constant to this number, so a change of one fails the other.
const READ_BOUND_SECS: u64 = 8;

/// How old the preview's title says its text is, in seconds: the `(Ns ago)` of ` preview ...`, `Some(60 * m)`
/// for minutes, `None` when the title says nothing of age.
fn title_age(screen: &str) -> Option<u64> {
    let line = screen.lines().find(|line| line.contains(" preview"))?;
    let after = line.split('(').nth(1)?;
    let (number, rest) = after.split_at(after.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = number.parse().ok()?;
    if rest.starts_with("s ago)") {
        Some(n)
    } else if rest.starts_with("m ago)") {
        Some(n * 60)
    } else {
        None
    }
}

// ---------------------------------------------------------------- the preview, end to end

/// The far side is the real far taw behind the stand-in: the flags the preview sends are the verb's own, or
/// the far side would refuse them and nothing would be drawn. The colour its pane printed is drawn red.
#[test]
fn a_machines_pane_is_previewed_in_the_colour_it_printed() {
    let _own = Owned::new();
    let rig = rig();
    far_runs(&rig, "printf '\\033[1;31mcrimson\\033[0m\\n'");
    let pid = start_app(&rig);
    let _ = pid;

    move_to(&rig, "0:main");
    expect(&rig, "the far pane's red word in the preview", || {
        word_in_red(&styled_screen(&rig.term), "crimson")
    });
    let asked = reads(&rig.stand_in);
    assert!(
        asked.iter().all(|line| line
            .ends_with(" taw 'pane' 'read' '%0' '--source' 'visible' '--ansi'")
            || line.contains("' '--source' 'visible' '--ansi'")),
        "{asked:?}\n{}",
        seen(&rig)
    );
    let far_pane = rig.far.server.pane_of("garden:main");
    assert!(
        asked
            .iter()
            .any(|line| line.contains(&format!("'read' '{far_pane}' '--source'"))),
        "the raw pane id, not studio:{far_pane}: {asked:?}"
    );
}

/// Text is data. The far side's answer holds a clipboard write, a title, a cursor move, a device control string
/// and a reordering character among its words, and none of it reaches the terminal: its words and its colour do. (A real far pane cannot say it: tmux eats such
/// sequences in the pane, so the stand-in answers this one itself.)
#[test]
fn a_far_screen_holding_a_clipboard_write_gives_the_terminal_only_its_words() {
    let _own = Owned::new();
    let rig = rig();
    rig.stand_in.reply_verb(
        "pane",
        "{\"id\":\"cli:pane:read\",\"result\":{\"pane_id\":\"x\",\"text\":\"\\u001b[1;31mred\\u001b[0m \\u001b]52;c;aGk=\\u0007after\\u001b]0;a title\\u0007\\n\\u001b[2J\\u001b[3;4Hmoved\\u001bP1;2|dcs-payload\\u001b\\\\tail\\u202ertl-payload\"}}\n",
    );
    start_app(&rig);

    move_to(&rig, "0:main");
    expect(&rig, "the far screen's words", || {
        let styled = styled_screen(&rig.term);
        styled.contains("after") && styled.contains("moved") && styled.contains("tail")
    });
    let styled = styled_screen(&rig.term);
    assert!(
        word_in_red(&styled, "red"),
        "{:?}",
        styled
            .lines()
            .filter(|line| line.contains("red"))
            .collect::<Vec<_>>()
    );
    // Only `clean::preview` takes a device control string's payload and a reordering character out: the
    // screen parser eats the clipboard write, the title and the cursor moves by itself.
    let plain = terminal_screen(&rig.term);
    assert!(
        !plain.contains("]52")
            && !plain.contains("aGk=")
            && !plain.contains("a title")
            && !plain.contains("dcs-payload")
            && !plain.contains('\u{202e}'),
        "{plain}"
    );
    let styled = styled_screen(&rig.term);
    assert!(
        !styled.contains("aGk=") && !styled.contains("dcs-payload") && !styled.contains('\u{202e}'),
        "{styled:?}"
    );
}

/// A hung read does not stop the list: the title says how old the text is while it hangs, and a key moves the
/// cursor.
#[test]
fn a_hung_read_ages_the_title_while_a_key_still_moves_the_cursor() {
    let _own = Owned::new();
    let rig = rig();
    far_runs(&rig, "printf '\\033[1;31mcrimson\\033[0m\\n'");
    start_app(&rig);
    move_to(&rig, "0:main");
    expect(&rig, "the far pane's word in the preview", || {
        terminal_screen(&rig.term).contains("crimson")
    });

    rig.stand_in.hang_verb("pane");

    // Every age the title says while the read hangs. A loop that waited for the read would jump from under
    // two seconds straight to the bound (8 s) or more, so the first age of two or more is well under it.
    let mut ages = Vec::new();
    expect(
        &rig,
        "a title that says the text is 2 s old or more",
        || {
            let age = title_age(&terminal_screen(&rig.term));
            ages.extend(age);
            age.is_some_and(|age| age >= 2)
        },
    );
    let first_old = ages.iter().copied().find(|age| *age >= 2).unwrap();
    assert!(
        first_old < READ_BOUND_SECS,
        "the title went from under 2 s to {first_old} s: the loop waited for the read. Ages seen: {ages:?}\n{}",
        seen(&rig)
    );
    assert!(
        terminal_screen(&rig.term).contains("crimson"),
        "the text stays"
    );
    let row = selected_row(&rig.term);
    press(&rig.term, &["j"]);
    expect(&rig, "the cursor moving a row while the read hangs", || {
        selected_row(&rig.term) != row
    });
}
