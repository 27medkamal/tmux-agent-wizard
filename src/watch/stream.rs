//! The watcher's view of the pane's byte stream.
//!
//! The bytes tmux pipes to us are an **activity signal**, not a transcript: the only
//! thing parsed out of them is the OSC `9;4` progress sequence (herdr's `osc_progress`
//! region). Everything else about the pane is read back from tmux at evaluation time
//! (`capture-pane`, `#{pane_title}`, …), so a partially-read chunk can never desync our
//! idea of the screen.

use std::time::Instant;

/// Longest partial OSC prefix carried between reads.
///
/// A complete progress sequence is at most ~14 bytes (`ESC ] 9 ; 4 ; 1 ; 100 BEL`), so
/// anything longer than this cap cannot become one: an unterminated tail over the cap is
/// dropped rather than carried, which bounds the memory a hostile stream can pin here.
const CARRY_CAP: usize = 64;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// What the watcher knows from the byte stream alone.
#[derive(Debug, Default)]
pub struct StreamState {
    /// When the last chunk of pane output arrived; `None` until the first byte. For the
    /// debug log only: nothing is scheduled from it.
    pub last_output_at: Option<Instant>,
    /// Last complete OSC `9;4` payload, formatted `4;<state>[;<pct>]` (empty = unknown).
    pub osc_progress: String,
    /// Bytes since the last read of the pane. Output since the last read is what makes a
    /// pane eligible for a read at its next tick — herdr's `detection_content_seq`, which
    /// rises on every non-empty PTY read — and it does nothing else: it never sets or
    /// holds a state. Reset only by a read ([`StreamState::mark_read`]).
    pub bytes_since_eval: u64,
    /// When the first of those bytes arrived. The read they are owed falls on the first
    /// tick at or after this instant, however much more output follows, so a turn that
    /// never stops redrawing is still read on every tick.
    pub eligible_since: Option<Instant>,
    /// Trailing bytes of an OSC sequence split across two reads.
    carry: Vec<u8>,
}

impl StreamState {
    /// Folds one chunk of pane output in: bumps the activity clock and byte counter,
    /// makes the pane eligible for a read if nothing had since the last one, and updates
    /// [`StreamState::osc_progress`] when the chunk completed a progress sequence.
    /// Returns `true` when `osc_progress` changed.
    pub fn feed(&mut self, buf: &[u8], now: Instant) -> bool {
        self.last_output_at = Some(now);
        self.bytes_since_eval = self.bytes_since_eval.saturating_add(buf.len() as u64);
        if !buf.is_empty() && self.eligible_since.is_none() {
            self.eligible_since = Some(now);
        }
        match scan_osc_progress(buf, &mut self.carry) {
            Some(progress) if progress != self.osc_progress => {
                self.osc_progress = progress;
                true
            }
            _ => false,
        }
    }

    /// Something other than output says the pane is worth a read at its next tick: an agent's
    /// report (see `watch::nudge`). It adds no bytes and leaves the activity clock alone, since the
    /// pane printed nothing, and makes the pane eligible as the first byte of output does.
    pub fn nudge(&mut self, now: Instant) {
        if self.eligible_since.is_none() {
            self.eligible_since = Some(now);
        }
    }

    /// A read of the pane has happened: the output so far has been seen, and none is
    /// owed a read any more. The progress sequence is a fact about the pane, so it stays.
    pub fn mark_read(&mut self) {
        self.bytes_since_eval = 0;
        self.eligible_since = None;
    }

    /// The agent that printed the last progress sequence has left the pane (herdr drops
    /// a departed agent's OSC evidence): until the next agent prints one of its own, the
    /// pane has none. A sequence already half-read is the next agent's, so it is kept.
    pub fn forget_progress(&mut self) {
        self.osc_progress.clear();
    }
}

/// The last complete OSC `9;4` progress payload in `carry ++ buf`, formatted
/// `4;<state>[;<pct>]` — the shape herdr's `osc_progress` region matches against.
///
/// A sequence is `ESC ] 9 ; 4 ; <state> [; <pct>] (BEL | ESC \)`; `<state>` and `<pct>`
/// must be decimal, so a malformed payload is ignored rather than written to tmux. Any
/// other OSC (`ESC ] 2 ; title BEL`, a plain OSC 9 notification, …) is skipped.
///
/// `carry` makes the scan resumable: a sequence cut in half by a read boundary leaves its
/// prefix in `carry`, and the next call sees the whole thing. `carry` is emptied on every
/// call and only re-filled from the tail of this chunk (never beyond [`CARRY_CAP`]).
pub fn scan_osc_progress(buf: &[u8], carry: &mut Vec<u8>) -> Option<String> {
    let mut data = std::mem::take(carry);
    data.extend_from_slice(buf);

    let mut found = None;
    let mut i = 0;
    while i < data.len() {
        if data[i] != ESC {
            i += 1;
            continue;
        }
        if i + 1 == data.len() {
            // A lone trailing ESC may be the start of an OSC introducer.
            stash(carry, &data[i..]);
            return found;
        }
        if data[i + 1] != b']' {
            // Not an OSC introducer. Advance by one, not two: the next byte could
            // itself be the ESC of a real introducer (`ESC ESC ] …`).
            i += 1;
            continue;
        }
        match terminator(&data[i + 2..]) {
            Some((payload_len, term_len)) => {
                if let Some(progress) = progress_payload(&data[i + 2..i + 2 + payload_len]) {
                    found = Some(progress);
                }
                // `term_len == 0` means a stray ESC ended the string: resume *at* it so
                // the loop can read it as the introducer of the next sequence.
                i += 2 + payload_len + term_len;
            }
            None => {
                // Unterminated: everything from here on belongs to this sequence, so
                // there is no later complete one to find.
                stash(carry, &data[i..]);
                return found;
            }
        }
    }
    found
}

/// `(payload length, terminator length)` for the OSC string starting at the front of
/// `rest`, or `None` when the string is not terminated within `rest` (more bytes needed).
///
/// A terminator is `BEL` (1 byte) or `ESC \` (2). A stray `ESC` followed by anything else
/// cancels the string, reported as a zero-length terminator so the caller resumes on that
/// `ESC` rather than swallowing it.
fn terminator(rest: &[u8]) -> Option<(usize, usize)> {
    let mut j = 0;
    while j < rest.len() {
        match rest[j] {
            BEL => return Some((j, 1)),
            ESC if j + 1 == rest.len() => return None, // ST or cancel? need one more byte
            ESC if rest[j + 1] == b'\\' => return Some((j, 2)),
            ESC => return Some((j, 0)),
            _ => j += 1,
        }
    }
    None
}

/// `Some("4;<state>[;<pct>]")` when `payload` is an OSC `9;4` progress body, else `None`.
fn progress_payload(payload: &[u8]) -> Option<String> {
    let rest = std::str::from_utf8(payload).ok()?.strip_prefix("9;4;")?;
    let mut fields = rest.split(';');
    let state = fields.next()?;
    if !is_decimal(state) {
        return None;
    }
    if let Some(pct) = fields.next()
        && !is_decimal(pct)
    {
        return None;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(format!("4;{rest}"))
}

fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn stash(carry: &mut Vec<u8>, tail: &[u8]) {
    carry.clear();
    if tail.len() <= CARRY_CAP {
        carry.extend_from_slice(tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(buf: &[u8]) -> Option<String> {
        scan_osc_progress(buf, &mut Vec::new())
    }

    #[test]
    fn a_nudge_makes_the_pane_eligible_as_output_does_and_counts_no_output() {
        let base = Instant::now();
        let mut stream = StreamState::default();
        stream.nudge(base);
        assert_eq!(stream.eligible_since, Some(base));
        assert_eq!(stream.bytes_since_eval, 0, "a report is no bytes");
        assert_eq!(stream.last_output_at, None, "the pane printed nothing");
        // The first of what is owed a read is the one that counts, however many come after.
        let later = base + std::time::Duration::from_millis(250);
        stream.nudge(later);
        stream.feed(b"x", later);
        assert_eq!(stream.eligible_since, Some(base));
        // A read pays all of it.
        stream.mark_read();
        assert_eq!(stream.eligible_since, None);
    }

    #[test]
    fn reads_a_bel_terminated_progress_sequence() {
        assert_eq!(scan(b"\x1b]9;4;1;40\x07").as_deref(), Some("4;1;40"));
    }

    #[test]
    fn reads_an_st_terminated_progress_sequence() {
        assert_eq!(scan(b"\x1b]9;4;0\x1b\\").as_deref(), Some("4;0"));
    }

    #[test]
    fn reads_a_sequence_surrounded_by_ordinary_output() {
        assert_eq!(
            scan(b"before\r\n\x1b]9;4;3;7\x07after\r\n").as_deref(),
            Some("4;3;7")
        );
    }

    #[test]
    fn ignores_osc_sequences_that_are_not_progress() {
        assert_eq!(scan(b"\x1b]2;\xe2\x9c\xb3 fake task\x07"), None);
        assert_eq!(scan(b"\x1b]0;title\x07"), None);
        assert_eq!(scan(b"\x1b]9;a plain notification\x07"), None);
        assert_eq!(scan(b"\x1b]99;4;1\x07"), None);
    }

    #[test]
    fn ignores_a_malformed_progress_payload() {
        assert_eq!(scan(b"\x1b]9;4;x\x07"), None);
        assert_eq!(scan(b"\x1b]9;4;\x07"), None);
        assert_eq!(scan(b"\x1b]9;4;1;40;9\x07"), None);
    }

    #[test]
    fn ignores_plain_output_and_leaves_no_carry() {
        let mut carry = Vec::new();
        assert_eq!(scan_osc_progress(b"just some output\r\n", &mut carry), None);
        assert!(carry.is_empty());
    }

    #[test]
    fn returns_the_last_complete_sequence_of_the_chunk() {
        assert_eq!(
            scan(b"\x1b]9;4;1;10\x07 work \x1b]9;4;1;90\x07").as_deref(),
            Some("4;1;90")
        );
    }

    #[test]
    fn resumes_a_sequence_split_across_two_reads() {
        let mut carry = Vec::new();
        assert_eq!(scan_osc_progress(b"out\x1b]9;4;1;4", &mut carry), None);
        assert_eq!(carry, b"\x1b]9;4;1;4");
        assert_eq!(
            scan_osc_progress(b"0\x07rest", &mut carry).as_deref(),
            Some("4;1;40")
        );
        assert!(carry.is_empty());
    }

    #[test]
    fn resumes_a_sequence_split_on_the_escape_byte() {
        let mut carry = Vec::new();
        assert_eq!(scan_osc_progress(b"out\x1b", &mut carry), None);
        assert_eq!(carry, b"\x1b");
        assert_eq!(
            scan_osc_progress(b"]9;4;2\x07", &mut carry).as_deref(),
            Some("4;2")
        );
    }

    #[test]
    fn resumes_a_sequence_split_on_the_string_terminator() {
        let mut carry = Vec::new();
        assert_eq!(scan_osc_progress(b"\x1b]9;4;0\x1b", &mut carry), None);
        assert_eq!(
            scan_osc_progress(b"\\more", &mut carry).as_deref(),
            Some("4;0")
        );
    }

    #[test]
    fn caps_the_carry_so_an_unterminated_sequence_cannot_grow_without_bound() {
        let mut carry = Vec::new();
        let mut chunk = b"\x1b]9;4;".to_vec();
        chunk.extend(std::iter::repeat_n(b'1', 500));
        assert_eq!(scan_osc_progress(&chunk, &mut carry), None);
        assert!(carry.len() <= 64, "carry grew to {}", carry.len());
    }

    #[test]
    fn a_cancelling_escape_does_not_hide_the_next_sequence() {
        // `ESC ] 9 ; 4 ; 1` cancelled by a CSI, then a real sequence.
        assert_eq!(
            scan(b"\x1b]9;4;1\x1b[0m\x1b]9;4;0\x07").as_deref(),
            Some("4;0")
        );
    }

    #[test]
    fn the_first_byte_after_a_read_makes_the_pane_eligible_until_the_next_read() {
        // The scheduler owes a read at the tick at or after the *first* byte since the
        // last read. Were later bytes to move that instant, a turn that never stops
        // redrawing would push its read back for ever — the 2 s wait this replaced.
        let start = Instant::now();
        let mut state = StreamState::default();
        assert_eq!(state.eligible_since, None, "no output yet, nothing owed");

        let first = start + std::time::Duration::from_millis(10);
        state.feed(b"\x1b[2J", first);
        state.feed(b"spinner", start + std::time::Duration::from_millis(60));
        assert_eq!(state.eligible_since, Some(first));
        assert_eq!(state.bytes_since_eval, 11);

        state.mark_read();
        assert_eq!(state.eligible_since, None, "a read has seen that output");
        assert_eq!(state.bytes_since_eval, 0);
        // The progress sequence is a fact about the pane, not about one read.
        state.feed(
            b"\x1b]9;4;1;40\x07",
            start + std::time::Duration::from_millis(90),
        );
        state.mark_read();
        assert_eq!(state.osc_progress, "4;1;40");

        let later = start + std::time::Duration::from_millis(400);
        state.feed(b".", later);
        assert_eq!(state.eligible_since, Some(later));
    }

    #[test]
    fn a_departed_agents_progress_is_forgotten_and_the_next_one_is_read() {
        let mut state = StreamState::default();
        state.feed(b"\x1b]9;4;3\x07", Instant::now());
        assert_eq!(state.osc_progress, "4;3");
        state.forget_progress();
        assert_eq!(
            state.osc_progress, "",
            "no progress until the next agent prints one"
        );
        // Its successor's sequence, split across two chunks around the reset, is read.
        state.feed(b"\x1b]9;4;", Instant::now());
        state.forget_progress();
        assert!(state.feed(b"0\x07", Instant::now()));
        assert_eq!(state.osc_progress, "4;0");
    }

    #[test]
    fn feed_tracks_activity_bytes_and_progress() {
        let mut state = StreamState::default();
        assert!(state.last_output_at.is_none());
        let changed = state.feed(b"\x1b]9;4;1;40\x07", Instant::now());
        assert!(changed);
        assert_eq!(state.osc_progress, "4;1;40");
        assert_eq!(state.bytes_since_eval, 11);
        assert!(state.last_output_at.is_some());
        // Ordinary output keeps the last known progress and does not report a change.
        assert!(!state.feed(b"more output", Instant::now()));
        assert_eq!(state.osc_progress, "4;1;40");
        assert_eq!(state.bytes_since_eval, 22);
    }
}
