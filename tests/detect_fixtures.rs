//! Claude screens, matched against the bundled rules.
//!
//! These pin the one thing that must not drift: that the vendored manifests still
//! produce herdr's verdicts on real terminal output.
//!
//! `claude-permission.txt` is hand-edited, not captured: the dialog Claude Code draws
//! is boxed, and `claude-permission-boxed.txt` keeps that captured form. The box makes
//! every `visible_blocker` rule in `claude.toml` unreachable, so the unboxed copy
//! exists to exercise the evidence flags at all. Both are asserted below; the boxed
//! one pins the gap.

use taw::detect::{AgentState, ManifestSource, Snapshot, evaluate, identify, parse_manifest};

fn claude() -> taw::detect::LoadedManifest {
    parse_manifest(
        include_str!("../src/detect/manifests/claude.toml"),
        ManifestSource::Bundled,
    )
    .unwrap()
}

/// The bundled Claude manifest plus taw's own supplement rules, exactly as
/// `ManifestStore::load` produces with `[detection] supplements = true` (the
/// default).
fn claude_with_supplement() -> taw::detect::LoadedManifest {
    let mut manifest = claude();
    manifest
        .append_supplement(include_str!("../src/detect/supplements/claude.toml"))
        .unwrap();
    manifest
}

fn screen(name: &str) -> String {
    std::fs::read_to_string(format!("tests/fixtures/screens/{name}")).unwrap()
}

/// `osc_title` for every fixture test below that is not itself testing an OSC rule:
/// the idle-looking static prefix, so the OSC title contributes only an idle signal
/// at priority 250, which any real screen rule (every claude.toml rule at or above
/// priority 900) still outranks — the same convention
/// `claude_2_1_241_dot_spinner_is_idle_with_upstream_rules_and_working_with_override`
/// above already uses.
const FAKE_IDLE_OSC_TITLE: &str = "✳ fake task";

fn evaluate_screen(
    manifest: &taw::detect::LoadedManifest,
    fixture: &str,
) -> taw::detect::Evaluation {
    let s = screen(fixture);
    evaluate(
        manifest,
        &Snapshot {
            content: &s,
            osc_title: FAKE_IDLE_OSC_TITLE,
            osc_progress: "",
        },
    )
}

#[test]
fn claude_idle_prompt_box_is_idle() {
    let s = screen("claude-idle.txt");
    let e = evaluate(
        &claude(),
        &Snapshot {
            content: &s,
            osc_title: "✳ fake task",
            osc_progress: "",
        },
    );
    assert_eq!(e.state, AgentState::Idle);
    assert_eq!(e.matched_rule.unwrap().id, "live_prompt_box");
}

#[test]
fn claude_2_1_227_spinner_is_working() {
    let s = screen("claude-2.1.227-working.txt");
    let e = evaluate(
        &claude(),
        &Snapshot {
            content: &s,
            osc_title: "✻ fake task",
            osc_progress: "",
        },
    );
    assert_eq!(e.state, AgentState::Working);
    assert_eq!(e.matched_rule.unwrap().id, "live_turn_working");
}

#[test]
fn claude_permission_prompt_is_blocked() {
    let s = screen("claude-permission.txt");
    let e = evaluate(
        &claude(),
        &Snapshot {
            content: &s,
            osc_title: "✳ fake task",
            osc_progress: "",
        },
    );
    assert_eq!(e.state, AgentState::Blocked);
    assert!(e.visible_blocker, "matched {:?}", e.matched_rule);
}

/// Pins a gap in herdr's current Claude rules, so an upstream fix flips this test
/// instead of landing unnoticed.
///
/// Claude Code draws the permission dialog inside a `╭ │ ╰` box, and `│` is not
/// whitespace, so every `visible_blocker` rule — all of which gate on `^`-anchored
/// option lines such as `(?i)^\s*❯?\s*1\.\s*yes\b` — fails against a captured
/// screen. Only `legacy_no_prompt_blocker` (priority 300) is left, and it raises no
/// evidence flag: the state is right but nothing downstream can tell this apart from
/// prompt-like text in the scrollback. When herdr teaches those rules to skip a box
/// border, this test starts failing and the fix should be vendored rather than
/// worked around here.
#[test]
fn claude_boxed_permission_prompt_reaches_no_visible_blocker_rule() {
    let s = screen("claude-permission-boxed.txt");
    let e = evaluate(
        &claude(),
        &Snapshot {
            content: &s,
            osc_title: "✳ fake task",
            osc_progress: "",
        },
    );
    assert_eq!(e.state, AgentState::Blocked);
    assert!(!e.visible_blocker);
    assert_eq!(
        e.matched_rule.as_ref().map(|rule| rule.id.as_str()),
        Some("legacy_no_prompt_blocker")
    );
}

/// Claude's folder trust question, which it asks at startup in a folder it has not been told to
/// trust. `claude-2.1.286-trust-folder.txt` is laid out from the strings Claude Code 2.1.286 draws
/// its `TrustDialog` with (read from its binary): the "Accessing workspace:" panel, the safety
/// question, "No, exit" focused first, "Yes, I trust this folder", and the enter and escape chords
/// at its foot. A question waiting on the user is `blocked`, which is why its answer, `idle`, was
/// once taken for a finished turn (`watch::state`, step 11 of `decide`).
#[test]
fn claude_2_1_286_folder_trust_question_is_blocked() {
    for manifest in [claude(), claude_with_supplement()] {
        let e = evaluate_screen(&manifest, "claude-2.1.286-trust-folder.txt");
        assert_eq!(e.state, AgentState::Blocked);
        assert!(e.visible_blocker);
        assert_eq!(
            e.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("live_blocked_form")
        );
    }
}

/// The manifests taw compiles in are herdr's, byte for byte, and `third_party/herdr/manifests/`
/// keeps the same bytes for comparison. A refresh that reaches only one of the two, or a hand
/// edit to a bundled copy, fails here: taw's own rules belong in `src/detect/supplements/`.
#[test]
fn every_bundled_manifest_is_byte_identical_to_its_vendored_copy() {
    for id in taw::detect::bundled_ids() {
        let bundled = std::fs::read(format!("src/detect/manifests/{id}.toml")).unwrap();
        let vendored = std::fs::read(format!("third_party/herdr/manifests/{id}.toml"))
            .unwrap_or_else(|e| panic!("{id}: no vendored copy: {e}"));
        assert!(
            bundled == vendored,
            "{id}.toml differs between src/detect/manifests/ and third_party/herdr/manifests/"
        );
    }

    // Antigravity is bundled under the id `agy`; herdr's file name for it is kept beside it, and
    // that second copy has to carry the same bytes.
    let agy = std::fs::read("src/detect/manifests/agy.toml").unwrap();
    let antigravity = std::fs::read("third_party/herdr/manifests/antigravity.toml")
        .unwrap_or_else(|e| panic!("antigravity: no vendored copy: {e}"));
    assert!(
        agy == antigravity,
        "agy.toml differs from third_party/herdr/manifests/antigravity.toml"
    );
}

#[test]
fn every_bundled_manifest_parses() {
    for id in taw::detect::bundled_ids() {
        let text = std::fs::read_to_string(format!("src/detect/manifests/{id}.toml")).unwrap();
        parse_manifest(&text, ManifestSource::Bundled).unwrap_or_else(|e| panic!("{id}: {e}"));
    }
}

#[test]
fn identity_uses_command_or_hint_never_screen() {
    assert_eq!(identify("claude", None).as_deref(), Some("claude"));
    assert_eq!(identify("claude-code", None).as_deref(), Some("claude")); // alias in claude.toml
    assert_eq!(identify("docker", None), None);
    assert_eq!(
        identify("docker", Some("claude")).as_deref(),
        Some("claude")
    );
    assert_eq!(identify("zsh", None), None);
}

// ---------------------------------------------------------------------------
// Claude Code 2.1.258 — taw's own detection supplements. Captures for four gaps in herdr's published rules: the `●`
// spinner glyph, vim mode's `-- INSERT --` footer prefix, a background shell still
// running after a turn ends, and a Stop-hooks phase after Claude's own reply. A fifth,
// the `✳` frame of a wait for background agents, is drawn from the vim capture.
// ---------------------------------------------------------------------------

#[test]
fn claude_2_1_258_dot_spinner_is_working_with_supplement_idle_without() {
    let with_supplement =
        evaluate_screen(&claude_with_supplement(), "claude-2.1.258-working-dot.txt");
    assert_eq!(with_supplement.state, AgentState::Working);
    assert_eq!(
        with_supplement.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("taw_live_turn_working_dot")
    );

    let without_supplement = evaluate_screen(&claude(), "claude-2.1.258-working-dot.txt");
    assert_eq!(without_supplement.state, AgentState::Idle);
    assert_eq!(
        without_supplement
            .matched_rule
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("live_prompt_box"),
        "upstream behaviour is idle — if this fails, herdr fixed it upstream"
    );
}

#[test]
fn claude_2_1_258_vim_footer_working_turn_is_working_via_the_bundled_rule() {
    // The vim-mode `-- INSERT --` footer here only decorates the *auto-mode*
    // status line, not the live-turn spinner line, so this fixture's working
    // verdict comes from the existing bundled `live_turn_working` (its glyph class
    // already includes ✽ U+273D) — the supplement changes nothing for this screen.
    // It still pins the gap `taw_live_turn_working_vim_footer` exists for: a vim
    // footer prefix on the *spinner* line itself must not stop that line matching.
    let e = evaluate_screen(&claude_with_supplement(), "claude-2.1.258-working-vim.txt");
    assert_eq!(e.state, AgentState::Working);
    assert_eq!(
        e.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("live_turn_working")
    );
}

/// herdr 2026.09.11.1 (v0.9.1) added `✳` (U+2733) to `live_turn_working`'s spinner
/// class: Claude Code 2.1.258 draws that frame too, and the manifest before it read every
/// `✳` frame of a turn as the prompt box's idle — `done` in the background. The screen is
/// the vim capture with its `✽` frame swapped for `✳`, under a working title that no
/// title rule matches, so only the spinner line can say working.
#[test]
fn claude_2_1_258_asterisk_frame_is_working_with_the_bundled_manifest() {
    let s = screen("claude-2.1.258-working-vim.txt").replacen(
        "\u{273D} Combobulating\u{2026}",
        "\u{2733} Combobulating\u{2026}",
        1,
    );
    assert!(
        s.contains("\u{2733} Combobulating"),
        "the fixture's spinner line moved; this test no longer swaps its frame"
    );
    let e = evaluate(
        &claude(),
        &Snapshot {
            content: &s,
            osc_title: "\u{273B} fake task",
            osc_progress: "",
        },
    );
    assert_eq!(e.state, AgentState::Working);
    assert_eq!(
        e.matched_rule.as_ref().map(|rule| rule.id.as_str()),
        Some("live_turn_working")
    );
    assert!(e.visible_working);
}

/// The same `✳` frame, while Claude waits for its background agents. herdr's
/// `background_agents_working` reads that wait as working, but 2026.09.11.1 added `✳` only
/// to `live_turn_working`, and the wait line has no `…` for that rule to see: the prompt box
/// below it reads idle, which is `done` in the background while Claude is still waiting.
/// `taw_background_agents_working_asterisk` is the missing frame. The screen is the vim
/// capture with its spinner line replaced by the wait, under a title no title rule matches,
/// so only the wait line can say working.
#[test]
fn claude_background_agents_wait_in_the_asterisk_frame_is_working_with_supplement() {
    let frame = |glyph: char, wait: &str| {
        let s = screen("claude-2.1.258-working-vim.txt").replacen(
            "\u{273D} Combobulating\u{2026}",
            &format!("{glyph} {wait}"),
            1,
        );
        assert!(
            s.contains(wait),
            "the fixture's spinner line moved; this test no longer draws the wait"
        );
        s
    };
    let read = |manifest: &taw::detect::LoadedManifest, content: &str| {
        evaluate(
            manifest,
            &Snapshot {
                content,
                osc_title: "\u{273B} fake task",
                osc_progress: "",
            },
        )
    };
    let rule = |e: &taw::detect::Evaluation| e.matched_rule.as_ref().map(|r| r.id.clone());
    let two = "Waiting for 2 background agents to finish";

    // A frame herdr's class knows: the bundled rule reads the wait as working, so the
    // screen is the shape that rule expects.
    let known = read(&claude(), &frame('\u{2736}', two));
    assert_eq!(known.state, AgentState::Working);
    assert_eq!(rule(&known).as_deref(), Some("background_agents_working"));

    for wait in [two, "Waiting for 1 background agent to finish"] {
        let asterisk = frame('\u{2733}', wait);
        let upstream = read(&claude(), &asterisk);
        assert_eq!(upstream.state, AgentState::Idle, "{wait}");
        assert_eq!(
            rule(&upstream).as_deref(),
            Some("live_prompt_box"),
            "upstream behaviour is idle — if this fails, herdr fixed it upstream"
        );
        let supplemented = read(&claude_with_supplement(), &asterisk);
        assert_eq!(supplemented.state, AgentState::Working, "{wait}");
        assert_eq!(
            rule(&supplemented).as_deref(),
            Some("taw_background_agents_working_asterisk")
        );
        assert!(supplemented.visible_working);
    }
}

#[test]
fn claude_2_1_258_background_shell_is_working_with_supplement_idle_without() {
    let with_supplement = evaluate_screen(
        &claude_with_supplement(),
        "claude-2.1.258-shell-running.txt",
    );
    assert_eq!(with_supplement.state, AgentState::Working);
    assert_eq!(
        with_supplement.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("taw_background_shell_working")
    );

    let without_supplement = evaluate_screen(&claude(), "claude-2.1.258-shell-running.txt");
    assert_eq!(without_supplement.state, AgentState::Idle);
    assert_eq!(
        without_supplement
            .matched_rule
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("live_prompt_box"),
        "upstream behaviour is idle — if this fails, herdr fixed it upstream"
    );
}

#[test]
fn claude_2_1_258_background_shell_dialog_still_reads_blocked() {
    // Regression: a permission dialog over a still-running background shell must
    // keep reading blocked — `live_blocked_form` (priority 980) has to outrank the
    // new `taw_background_shell_working` (965), not the other way around.
    let e = evaluate_screen(
        &claude_with_supplement(),
        "claude-2.1.258-shell-running-dialog.txt",
    );
    assert_eq!(e.state, AgentState::Blocked);
    assert_eq!(
        e.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("live_blocked_form")
    );
}

#[test]
fn claude_2_1_258_continuing_after_a_background_command_is_working() {
    let e = evaluate_screen(&claude_with_supplement(), "claude-2.1.258-continuing.txt");
    assert_eq!(e.state, AgentState::Working);
}

/// A turn running its Stop hooks after Claude's own reply is still working, not
/// idle — an idle verdict there is exactly the false notification this project
/// exists to avoid, so `taw_stop_hooks_working` reads it directly by the literal
/// phrase rather than the spinner shape.
///
/// `live_turn_working` (bundled) cannot see this line on its own: the status line is
/// `✶ Mustering… (running stop hooks… 1/2 · 6s · ↓ 4 tokens)`, which has *two*
/// ellipses. Its bullet-glyph clause is `.*…(?:\s+\(\d+[smh](?:\s|·)|\s*$)` — greedy
/// `.*…` binds to the last ellipsis first, landing right before `1/2 · 6s …`, which
/// is not `\(\d+[smh]`, so that alternative fails; backtracking to the first
/// ellipsis then requires `\(\d+[smh]` right after `(`, but finds `(running`
/// instead. `taw_stop_hooks_working` sidesteps the timer shape entirely by matching
/// the literal phrase "running stop hooks".
#[test]
fn claude_2_1_258_stop_hooks_running_after_finish_is_working() {
    let e = evaluate_screen(&claude_with_supplement(), "claude-2.1.258-stop-hooks.txt");
    assert_eq!(e.state, AgentState::Working);
    assert_eq!(
        e.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("taw_stop_hooks_working")
    );
}

// ---------------------------------------------------------------------------
// Claude Code with sub-agents running: taw's own `taw_subagent_running`. While a Claude has sub-agents it lists them under the prompt
// box, one row each, and a main agent that has ended its turn is at its prompt with the
// list still counting. herdr reads that as idle, and the watcher writes `done` with the
// work unfinished. The layout is what a Claude draws, from a capture of one; the names and
// the words in the rows are made up.
// ---------------------------------------------------------------------------

#[test]
fn claude_prompt_box_with_a_running_sub_agent_is_working_with_supplement_idle_without() {
    let with_supplement =
        evaluate_screen(&claude_with_supplement(), "claude-subagents-running.txt");
    assert_eq!(with_supplement.state, AgentState::Working);
    assert_eq!(
        with_supplement.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("taw_subagent_running")
    );
    assert!(with_supplement.visible_working);

    let without_supplement = evaluate_screen(&claude(), "claude-subagents-running.txt");
    assert_eq!(without_supplement.state, AgentState::Idle);
    assert_eq!(
        without_supplement
            .matched_rule
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("live_prompt_box"),
        "upstream behaviour is idle, if this fails herdr fixed it upstream"
    );
}

#[test]
fn claude_prompt_box_with_a_sub_agent_that_has_only_started_is_working_with_supplement() {
    // An agent that has not counted a token yet shows its time alone, `3s`, at the end of
    // its row, in the list under `main`. The same layout as the running capture, with
    // the status of the last row shortened to its time.
    let with_supplement =
        evaluate_screen(&claude_with_supplement(), "claude-subagents-starting.txt");
    assert_eq!(with_supplement.state, AgentState::Working);
    assert_eq!(
        with_supplement.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("taw_subagent_running")
    );
    assert!(with_supplement.visible_working);

    let without_supplement = evaluate_screen(&claude(), "claude-subagents-starting.txt");
    assert_eq!(without_supplement.state, AgentState::Idle);
    assert_eq!(
        without_supplement
            .matched_rule
            .as_ref()
            .map(|r| r.id.as_str()),
        Some("live_prompt_box"),
        "upstream behaviour is idle, if this fails herdr fixed it upstream"
    );
}

#[test]
fn claude_prompt_box_with_every_sub_agent_idle_is_idle_with_supplement() {
    let e = evaluate_screen(&claude_with_supplement(), "claude-subagents-idle.txt");
    assert_eq!(e.state, AgentState::Idle);
    assert_eq!(
        e.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("live_prompt_box")
    );
    assert!(e.visible_idle);
}

#[test]
fn claude_dialog_under_a_running_sub_agent_list_still_reads_blocked() {
    // A dialog that does not take the prompt away is drawn under the box and its list, as
    // the background shell's dialog above is. The list is above its rule, and the blocker
    // is what reads.
    let mut dialog = screen("claude-subagents-running.txt");
    dialog.push_str(&"─".repeat(100));
    dialog.push_str(
        "\n Which parser should the builder use?\n\n ❯ 1. The old one\n   2. The new one\n\n Enter to confirm · Esc to cancel\n",
    );
    let e = evaluate(
        &claude_with_supplement(),
        &Snapshot {
            content: &dialog,
            osc_title: FAKE_IDLE_OSC_TITLE,
            osc_progress: "",
        },
    );
    assert_eq!(e.state, AgentState::Blocked);
    assert_eq!(
        e.matched_rule.as_ref().map(|r| r.id.as_str()),
        Some("live_blocked_form")
    );
    assert!(e.visible_blocker);
}
