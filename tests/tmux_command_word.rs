//! `taw::tmux::command_word` against a real tmux server: the word a command that tmux parses from a
//! string (the commands of `if-shell`) reads back as exactly the text it was made from.
//!
//! tmux's parser gives meaning inside double quotes to `\`, `$` and `~`, and drops the spaces after a
//! line break and reads a `#` there as a comment, so a text that goes into such a command (a prompt that
//! `taw agent prompt` types, which can hold any of them) has to be written so that none of them acts.
//! Each text here is put into a tmux buffer through the word and read back from it, which says what the
//! parser made of the word with no pane, tty or program in between.

mod common;

use common::Server;

/// `text` put into a tmux buffer through [`taw::tmux::command_word`], and read back. A bar is put after
/// the text, so that the buffer read back is not trimmed of what a test text ends with, and none before
/// it: a `~` that starts the quoted piece is the one tmux reads as a home directory.
fn round_trip(server: &Server, pane: &str, text: &str) -> String {
    let wrapped = format!("{text}|");
    server.tmux_ok(&[
        "if-shell",
        "-F",
        "-t",
        pane,
        "1",
        &format!(
            "set-buffer -b taw-round-trip -- {}",
            taw::tmux::command_word(&wrapped)
        ),
    ]);
    let read = server.tmux_ok(&["show-buffer", "-b", "taw-round-trip"]);
    read.strip_suffix('|')
        .unwrap_or_else(|| panic!("{read:?} does not end with the bar {wrapped:?} does"))
        .to_string()
}

#[test]
fn every_ascii_character_comes_back_as_it_went_in() {
    let server = Server::start();
    let pane = server.pane();
    for byte in 1u8..=126 {
        let text = (byte as char).to_string();
        assert_eq!(
            round_trip(&server, &pane, &text),
            text,
            "byte {byte:#04x} came back as something else"
        );
    }
}

#[test]
fn the_characters_tmux_reads_for_a_meaning_come_back_as_they_went_in() {
    let server = Server::start();
    let pane = server.pane();
    for text in [
        "",
        "plain text",
        r#"a\b \\ \" \n \t \\n A \101 \$HOME \~"#,
        r#"quote " and ' and `tick` and "double" and 'single'"#,
        "$HOME ${HOME} $(echo x) $1 $$ $",
        "~ ~/x ~root ~+ ~- a~b ~~",
        "#{pane_id} #(date) ## #S #[fg=red] 100% %d %%",
        "a;b ; c;; ;d; {x} } { ;} {;",
        "one\ntwo\n  indented\n\t# comment-looking\n    #{not a format}\n\n\nlast",
        "carriage\rreturn\r\nand\tbell",
        "-- -l -t %1 --",
        "caf\u{e9} \u{65e5}\u{672c}\u{8a9e} \u{1f600} \u{1f468}\u{200d}\u{1f4bb}",
        "trailing backslash \\",
        "\\\n  # a backslash before a line break\n",
        "  leading and trailing spaces  ",
        "if-shell -F 1 'kill-server'",
        "\"; kill-server; \"",
    ] {
        assert_eq!(round_trip(&server, &pane, text), text, "{text:?}");
    }
}

/// Texts made of what is special, mixed in an order nobody picked: a fixed generator, so a failure is
/// the same text each run.
#[test]
fn mixtures_of_what_is_special_come_back_as_they_went_in() {
    let server = Server::start();
    let pane = server.pane();
    let alphabet: Vec<&str> = vec![
        "\\",
        "\"",
        "'",
        "$",
        "~",
        "#",
        "{",
        "}",
        ";",
        " ",
        "\n",
        "\r",
        "\t",
        "%",
        "a",
        "Z",
        "0",
        "-",
        "\u{e9}",
        "\u{1f600}",
        "n",
        "t",
        "e",
        "x",
        "u",
    ];
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..200 {
        let length = 1 + (next() % 40) as usize;
        let text: String = (0..length)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        assert_eq!(round_trip(&server, &pane, &text), text, "{text:?}");
    }
}
