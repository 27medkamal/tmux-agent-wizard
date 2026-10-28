//! The Agents view projection: what parses, what herdr would refuse, and what a query
//! keeps and how it orders it.

use std::collections::BTreeMap;

use super::*;

/// One agent in workspace `$<space>`, window `@<space><window>`, pane `%<space><window>`.
fn agent(space: usize, window: u32, kind: &str, state: Option<AgentState>, since: u64) -> AgentRow {
    AgentRow {
        pane_id: format!("%{space}{window}"),
        session_id: format!("${space}"),
        session_name: format!("ws{space}"),
        window_id: format!("@{space}{window}"),
        window_index: window,
        pane_index: 0,
        kind: Some(kind.to_string()),
        state,
        state_since: since,
        space_order: space,
        ..Default::default()
    }
}

fn current() -> Current {
    Current {
        workspace_id: Some("w0".to_string()),
        tab_id: Some("w0:t00".to_string()),
    }
}

/// The names of the agents a view keeps, in the order it puts them in.
fn projected(view: &AgentView, rows: &[AgentRow]) -> Vec<String> {
    let mut order: Vec<usize> = (0..rows.len())
        .filter(|&i| view.matches(&rows[i], &current()))
        .collect();
    view.sort(rows, &mut order);
    order
        .into_iter()
        .map(|i| format!("{}/{}", rows[i].session_name, rows[i].window_index))
        .collect()
}

fn parsed(json: &str) -> AgentView {
    parse(Some(json))
        .unwrap_or_else(|e| panic!("expected {json} to parse: {e}"))
        .expect("a projection")
}

#[test]
fn an_unset_or_blank_option_is_no_projection() {
    assert_eq!(parse(None), Ok(None));
    assert_eq!(parse(Some("")), Ok(None));
    assert_eq!(parse(Some("   \n")), Ok(None));
}

/// The query from the "Agent view queries" section of herdr's socket API docs, verbatim
/// — every construct taw has to understand in one value.
#[test]
fn herdrs_documented_example_query_parses_and_projects() {
    let view = parsed(
        r#"{
          "source": "plugin:example.agent-views",
          "label": "focus",
          "filter": {
            "op": "any",
            "filters": [
              {"op": "eq", "field": "workspace_id", "value": {"context": "current_workspace_id"}},
              {"op": "in", "field": "status", "values": ["blocked", "done"]}
            ]
          },
          "sort": [
            {"field": "attention", "order": "desc"},
            {"field": "state_change_seq", "order": "desc"}
          ]
        }"#,
    );
    assert_eq!(view.source, "plugin:example.agent-views");
    assert_eq!(view.label(), "focus");

    let rows = [
        // In the current workspace, so it is kept whatever its status.
        agent(0, 0, "claude", Some(AgentState::Working), 100),
        // Elsewhere and idle: dropped.
        agent(1, 0, "claude", Some(AgentState::Idle), 300),
        // Elsewhere but blocked: kept, and it wants attention most.
        agent(2, 0, "codex", Some(AgentState::Blocked), 200),
        // Elsewhere and finished unseen: kept, below blocked, above working.
        agent(3, 0, "codex", Some(AgentState::Done), 400),
    ];
    assert_eq!(projected(&view, &rows), ["ws2/0", "ws3/0", "ws0/0"]);
}

#[test]
fn a_view_with_no_filter_keeps_every_agent_and_may_only_reorder() {
    let view = parsed(r#"{"source":"cli","sort":[{"field":"status"}]}"#);
    let rows = [
        agent(0, 0, "claude", Some(AgentState::Working), 1),
        agent(1, 0, "claude", Some(AgentState::Blocked), 2),
        agent(2, 0, "claude", None, 3),
    ];
    // Sorted by status name, ascending: blocked, unknown, working.
    assert_eq!(projected(&view, &rows), ["ws1/0", "ws2/0", "ws0/0"]);
}

#[test]
fn not_and_all_compose() {
    let view = parsed(
        r#"{"source":"cli","filter":{"op":"all","filters":[
             {"op":"not","filter":{"op":"eq","field":"status","value":"idle"}},
             {"op":"eq","field":"agent","value":"claude"}
           ]}}"#,
    );
    let rows = [
        agent(0, 0, "claude", Some(AgentState::Idle), 1),
        agent(1, 0, "claude", Some(AgentState::Working), 1),
        agent(2, 0, "codex", Some(AgentState::Working), 1),
    ];
    assert_eq!(projected(&view, &rows), ["ws1/0"]);
}

#[test]
fn a_context_value_resolves_against_the_tab_the_popup_was_opened_from() {
    let view = parsed(
        r#"{"source":"cli","filter":{"op":"eq","field":"tab_id","value":{"context":"current_tab_id"}}}"#,
    );
    let rows = [
        agent(0, 0, "claude", None, 0),
        agent(0, 1, "claude", None, 0),
    ];
    assert_eq!(projected(&view, &rows), ["ws0/0"]);

    // No current tab (no attached client): nothing resolves, so nothing matches — the
    // panel says "no matching agents" rather than quietly showing everything.
    let nowhere = Current::default();
    assert!(!view.matches(&rows[0], &nowhere));
}

#[test]
fn exists_tests_presence_and_a_token_filter_reads_agent_tokens() {
    let view = parsed(r#"{"source":"cli","filter":{"op":"exists","field":{"token":"role"}}}"#);
    let mut tagged = agent(0, 0, "claude", None, 0);
    tagged.tokens = BTreeMap::from([("role".to_string(), "reviewer".to_string())]);
    let rows = [tagged, agent(1, 0, "claude", None, 0)];
    assert_eq!(projected(&view, &rows), ["ws0/0"]);
    assert!(view.uses_tokens());

    let by_value = parsed(
        r#"{"source":"cli","filter":{"op":"eq","field":{"token":"role"},"value":"reviewer"}}"#,
    );
    assert_eq!(projected(&by_value, &rows), ["ws0/0"]);
    let other = parsed(
        r#"{"source":"cli","filter":{"op":"eq","field":{"token":"role"},"value":"builder"}}"#,
    );
    assert!(projected(&other, &rows).is_empty());
}

#[test]
fn a_sort_puts_missing_values_last_whichever_way_it_runs() {
    let rows = [
        agent(0, 0, "claude", Some(AgentState::Idle), 0), // no transition recorded
        agent(1, 0, "claude", Some(AgentState::Idle), 50),
        agent(2, 0, "claude", Some(AgentState::Idle), 90),
    ];
    let asc = parsed(r#"{"source":"cli","sort":[{"field":"state_change_seq"}]}"#);
    assert_eq!(projected(&asc, &rows), ["ws1/0", "ws2/0", "ws0/0"]);
    let desc = parsed(r#"{"source":"cli","sort":[{"field":"state_change_seq","order":"desc"}]}"#);
    assert_eq!(projected(&desc, &rows), ["ws2/0", "ws1/0", "ws0/0"]);
}

#[test]
fn a_sort_is_stable_so_an_undistinguished_agent_keeps_its_place() {
    let view = parsed(r#"{"source":"cli","sort":[{"field":"agent"}]}"#);
    let rows = [
        agent(0, 0, "claude", None, 0),
        agent(1, 0, "claude", None, 0),
        agent(2, 0, "claude", None, 0),
    ];
    assert_eq!(projected(&view, &rows), ["ws0/0", "ws1/0", "ws2/0"]);
}

#[test]
fn seen_filters_on_the_focus_flag_and_a_missing_agent_kind_does_not_exist() {
    let view = parsed(r#"{"source":"cli","filter":{"op":"eq","field":"seen","value":true}}"#);
    let mut seen = agent(0, 0, "claude", None, 0);
    seen.seen = true;
    let rows = [seen, agent(1, 0, "claude", None, 0)];
    assert_eq!(projected(&view, &rows), ["ws0/0"]);

    let has_kind = parsed(r#"{"source":"cli","filter":{"op":"exists","field":"agent"}}"#);
    let mut kindless = agent(2, 0, "claude", Some(AgentState::Working), 0);
    kindless.kind = None;
    assert!(!has_kind.matches(&kindless, &current()));
}

#[test]
fn the_label_falls_back_to_herdrs_word_and_the_source_is_optional() {
    let view = parsed(r#"{"filter":{"op":"exists","field":"agent"}}"#);
    assert_eq!(view.label(), DEFAULT_LABEL);
    assert_eq!(view.source, "");
}

#[test]
fn malformed_json_is_reported_without_serdes_line_and_column_tail() {
    let error = parse(Some("{not json")).expect_err("malformed");
    assert!(!error.contains("at line"), "{error}");
    let unknown =
        parse(Some(r#"{"source":"cli","filter":{"op":"between"}}"#)).expect_err("an unknown op");
    assert!(unknown.contains("between"), "{unknown}");
}

/// The contract with `taw agent view set`: **the reader is looser than the writer.**
///
/// Every query below is one herdr refuses at its API boundary, and so does the CLI. The
/// popup evaluates them anyway, for two reasons: `@agent_wizard_view` is a plain tmux
/// option that anything can write by hand, and a reader stricter than the writer would
/// blank the panel over a query the user had just been told was fine. Which of the two
/// is stricter has already changed once during development; this test is what keeps that
/// from mattering. See `validate`.
#[test]
fn a_query_the_writer_would_refuse_is_still_evaluated_not_ignored() {
    let rows = [
        agent(0, 0, "claude", Some(AgentState::Idle), 0),
        agent(1, 0, "codex", Some(AgentState::Blocked), 0),
    ];
    // An empty `all` is vacuously true, so it keeps everything; an empty `any` or `in`
    // has nothing to match, so it keeps nothing. The second element is how many of the
    // two agents survive.
    let cases = [
        (r#"{"source":"cli","filter":{"op":"all","filters":[]}}"#, 2),
        (r#"{"source":"cli","filter":{"op":"any","filters":[]}}"#, 0),
        (
            r#"{"source":"cli","filter":{"op":"in","field":"status","values":[]}}"#,
            0,
        ),
        // A status outside herdr's five never equals a real one.
        (
            r#"{"source":"cli","filter":{"op":"eq","field":"status","value":"busy"}}"#,
            0,
        ),
        // A context compared to the id field it does not name never matches.
        (
            r#"{"source":"cli","filter":{"op":"eq","field":"tab_id","value":{"context":"current_workspace_id"}}}"#,
            0,
        ),
        // Values of the wrong type for their field never match.
        (
            r#"{"source":"cli","filter":{"op":"eq","field":"seen","value":"yes"}}"#,
            0,
        ),
        (
            r#"{"source":"cli","filter":{"op":"eq","field":"state_change_seq","value":"7"}}"#,
            0,
        ),
        (
            r#"{"source":"cli","filter":{"op":"eq","field":"agent","value":true}}"#,
            0,
        ),
        // A token name the writer allows but herdr's charset does not: a lookup that
        // finds nothing, not an error.
        (
            r#"{"source":"cli","filter":{"op":"exists","field":{"token":"no spaces"}}}"#,
            0,
        ),
        // A source that is not an identifier. The popup never interprets `source`.
        (
            r#"{"source":"a source","filter":{"op":"exists","field":"agent"}}"#,
            2,
        ),
    ];
    for (json, kept) in cases {
        let view = parse(Some(json))
            .unwrap_or_else(|e| panic!("{json} must be applied, not refused: {e}"))
            .expect("a projection");
        assert_eq!(projected(&view, &rows).len(), kept, "{json}");
    }

    // Sixty-five leaves and nine sort keys: past herdr's node and sort caps, still drawn.
    let leaves = [r#"{"op":"exists","field":"agent"}"#; 65].join(",");
    let wide = format!(r#"{{"source":"cli","filter":{{"op":"any","filters":[{leaves}]}}}}"#);
    assert_eq!(projected(&parsed(&wide), &rows).len(), 2);
    let sorts = [r#"{"field":"attention"}"#; 9].join(",");
    assert!(parse(Some(&format!(r#"{{"source":"cli","sort":[{sorts}]}}"#))).is_ok());

    // Twenty levels of nesting: far past herdr's cap of 8, still inside the reader's
    // stack guard.
    let mut deep = r#"{"op":"exists","field":"agent"}"#.to_string();
    for _ in 0..20 {
        deep = format!(r#"{{"op":"not","filter":{deep}}}"#);
    }
    assert!(
        parse(Some(&format!(r#"{{"source":"cli","filter":{deep}}}"#))).is_ok(),
        "the reader must be looser than the writer, not tighter"
    );
}

/// A label is shown on one border line, so it is cleaned rather than refused; one left
/// empty by cleaning falls back to herdr's word.
#[test]
fn a_label_is_trimmed_of_control_characters_and_length() {
    // Control characters are dropped rather than replaced, which is what herdr's own
    // `normalize_label` does — so the two agree even on a label nobody meant to write.
    let view = parsed(r#"{"source":"cli","label":"  re\tview\nqueue  "}"#);
    assert_eq!(view.label(), "reviewqueue");
    let blank = parsed(r#"{"source":"cli","label":"   "}"#);
    assert_eq!(blank.label(), DEFAULT_LABEL);
    let long = parsed(&format!(
        r#"{{"source":"cli","label":"{}"}}"#,
        "x".repeat(50)
    ));
    assert_eq!(long.label().chars().count(), 32);
}

/// What is still refused: a value with no query in it at all. There is nothing to
/// evaluate and nothing to draw, so the popup says so and shows every agent.
#[test]
fn only_a_value_that_holds_no_query_is_refused() {
    let refused = [
        "{not json",
        r#"{"source":"cli","filter":{"op":"between","field":"status"}}"#,
        r#"{"source":"cli","filter":"blocked"}"#,
        r#"{"source":"cli","filter":{"op":"eq","field":"status"}}"#,
        r#"{"source":"cli","filter":{"op":"eq","field":"nonesuch","value":"x"}}"#,
        r#"{"source":"cli","filter":{"op":"eq","field":"status","value":-1}}"#,
        r#"{"source":"cli","filter":{"op":"all"}}"#,
        r#"{"source":"cli","sort":[{"field":"whenever"}]}"#,
    ];
    for json in refused {
        assert!(parse(Some(json)).is_err(), "expected a refusal for {json}");
    }
}

#[test]
fn a_filter_nested_past_the_readers_stack_guard_is_refused_without_overflowing() {
    let mut json = r#"{"op":"exists","field":"agent"}"#.to_string();
    for _ in 0..64 {
        json = format!(r#"{{"op":"not","filter":{json}}}"#);
    }
    let error = parse(Some(&format!(r#"{{"source":"cli","filter":{json}}}"#)))
        .expect_err("past the depth guard");
    assert!(error.contains("nests deeper"), "{error}");
}

#[test]
fn uses_tokens_is_false_for_a_query_that_reads_only_built_ins() {
    let view = parsed(
        r#"{"source":"cli","filter":{"op":"exists","field":"agent"},"sort":[{"field":"status"}]}"#,
    );
    assert!(!view.uses_tokens());
    let sorting = parsed(r#"{"source":"cli","sort":[{"field":{"token":"role"}}]}"#);
    assert!(sorting.uses_tokens());
}
