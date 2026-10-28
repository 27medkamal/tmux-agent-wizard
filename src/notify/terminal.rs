//! `[ui.toast] delivery = "terminal"`: OSC 777 + OSC 9 notifications written straight
//! to each attached client's tty (works over ssh — no dependency on tmux's own
//! `display-message`).

use super::strip_ascii_control;
use crate::tmux::Tmux;
use std::fs::OpenOptions;
use std::io::Write;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Builds the OSC 777 + OSC 9 payload for `title`/`body`: `ESC ] 777 ; notify ;
/// <title> ; <body> BEL` followed by `ESC ] 9 ; <title>: <body> BEL`. Both inputs have
/// control characters (`0x00`-`0x1F`, `0x7F`) stripped first — a stray `ESC` or `BEL`
/// inside either would otherwise terminate the sequence early or start a new one.
/// `;` is additionally stripped from `title` only: OSC 777's fields are
/// semicolon-delimited (`notify;<title>;<body>`), so a `;` inside `title` would shift
/// which text lands in which field; `body` is the last field before the terminator, so
/// a `;` there is harmless and is kept.
///
/// OSC 9 has a single text field, so a `body` with nothing visible in it (empty, or only
/// spaces once the control characters are gone) leaves out the colon that joins it to the
/// title, and the sequence is `ESC ] 9 ; <title> BEL` instead of one ending on a colon and
/// a space. OSC 777 has a field of its own for the body and keeps it as it is.
pub fn osc_payload(title: &str, body: &str) -> Vec<u8> {
    let title = strip_ascii_control(title).replace(';', "");
    let body = strip_ascii_control(body);
    let osc_9_text = if body.trim().is_empty() {
        title.clone()
    } else {
        format!("{title}: {body}")
    };
    let mut out = Vec::new();
    out.extend_from_slice(&[ESC, b']']);
    out.extend_from_slice(format!("777;notify;{title};{body}").as_bytes());
    out.push(BEL);
    out.extend_from_slice(&[ESC, b']']);
    out.extend_from_slice(format!("9;{osc_9_text}").as_bytes());
    out.push(BEL);
    out
}

/// Writes `payload` to every attached client's `#{client_tty}`, opened
/// write-only (the client is reading its own tty as a terminal; this is the same
/// trick `tmux display-message` and shell prompts use to poke a terminal directly).
/// Returns how many clients were successfully written to. One client's tty being
/// unwritable (permissions, a stale/dead tty) never fails the whole call — the other
/// clients still get the notification.
pub fn write_to_clients(t: &Tmux, payload: &[u8]) -> anyhow::Result<usize> {
    let clients = t.list_clients()?;
    let mut successes = 0usize;
    for client in clients {
        #[cfg(test)]
        {
            let tty = std::path::Path::new(&client.client_tty);
            assert!(
                !tty.starts_with("/dev/") || tty == std::path::Path::new("/dev/null"),
                "a unit test must never write to a real terminal: {} is a device, list a file of the test's own",
                client.client_tty
            );
        }
        let wrote = OpenOptions::new()
            .write(true)
            .open(&client.client_tty)
            .and_then(|mut f| f.write_all(payload));
        if wrote.is_ok() {
            successes += 1;
        }
    }
    Ok(successes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_exact_osc_777_and_osc_9_payload() {
        let payload = osc_payload("t", "b");
        assert_eq!(payload, b"\x1b]777;notify;t;b\x07\x1b]9;t: b\x07".to_vec());
    }

    #[test]
    fn strips_escape_characters_from_title_and_body() {
        let payload = osc_payload("t\x1bx", "b\x1by");
        assert_eq!(
            payload,
            b"\x1b]777;notify;tx;by\x07\x1b]9;tx: by\x07".to_vec()
        );
    }

    #[test]
    fn strips_semicolons_from_title_but_keeps_them_in_body() {
        let payload = osc_payload("t;x", "b;y");
        assert_eq!(
            payload,
            b"\x1b]777;notify;tx;b;y\x07\x1b]9;tx: b;y\x07".to_vec()
        );
    }

    #[test]
    fn strips_other_ascii_control_bytes_such_as_bel() {
        let payload = osc_payload("t\x07x", "b");
        assert_eq!(
            payload,
            b"\x1b]777;notify;tx;b\x07\x1b]9;tx: b\x07".to_vec()
        );
    }

    #[test]
    fn osc_9_ends_on_the_title_when_there_is_no_body() {
        // OSC 9 is one text field, so title and body are joined with a colon, and with no
        // body that would end on a colon and a space. OSC 777 has a field of its own for the
        // body and keeps it, empty.
        let payload = osc_payload("t", "");
        assert_eq!(payload, b"\x1b]777;notify;t;\x07\x1b]9;t\x07".to_vec());
    }

    #[test]
    fn a_body_with_nothing_visible_in_it_is_no_body_in_osc_9() {
        // Control characters are stripped before anything is sent, so a body made only of
        // them is as empty as none, and so is one that is only spaces.
        for body in [" ", "   ", "\n", "\x07", "\x1b\x07", " \x07 "] {
            let payload = osc_payload("t", body);
            assert!(
                payload.ends_with(b"\x1b]9;t\x07"),
                "the body {body:?}: {}",
                payload.escape_ascii()
            );
        }
    }
}
