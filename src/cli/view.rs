//! `taw agent view set | clear` — herdr's `agent.view.set` / `agent.view.clear`.
//!
//! A **view** is a projection a plugin asks the agent list to show: "only the panes in
//! this workspace", "only the ones waiting on me", ordered by attention, under a label
//! naming who asked. herdr keeps it in the running server; here it lives in one global
//! tmux option, which is what makes it visible to the popup process (`taw ui`) without
//! any shared memory between them.
//!
//! ```text
//! taw agent view set --source review --label "Review queue" \
//!   --filter '{"op":"eq","field":"status","value":"blocked"}' \
//!   --sort '[{"field":"attention","order":"desc"}]'
//!   → @agent_wizard_view = {"source":…,"label":…,"filter":{…},"sort":[…]}
//! taw ui  → reads @agent_wizard_view, applies the projection
//! taw agent view clear  → the option is unset, meaning "no projection"
//! ```
//!
//! ## The option's contract
//!
//! `@agent_wizard_view` holds **exactly herdr's `agent.view.set` params JSON** —
//! `{"source", "label", "filter", "sort"}`, with everything but `source` omitted when not
//! given — and *unset* means no projection. Nothing else may be stored there. It is a
//! global option, not a session or pane one: a view is a property of the whole agent
//! list, and the popup is a `display-popup` in whichever session the user happened to be
//! in.
//!
//! `sort` is the **agents panel's** order, which is taw's own to set (`[ui]
//! agent_panel_sort`); a view replaces that policy for as long as it lasts. It has
//! nothing to do with workspace ordering.
//!
//! ## Why this file owns the rules
//!
//! herdr splits the work: its API validates a view once, on the way in
//! (`herdr-src/src/app/agent_view.rs`, `validate_agent_view`), and its renderer then
//! evaluates whatever survived without checking anything (`apply_agent_view`). taw is
//! split the same way across two *processes* — this verb is the API side, the popup is the
//! renderer — so the rules have to live here, and they are herdr's, rule for rule and
//! limit for limit.
//!
//! Being looser than herdr here is not a small thing: a query herdr rejects but this
//! accepts would be written to the option and reported as `active`, then dropped by the
//! popup — telling the user their own successful command did nothing. So this reproduces
//! every one of herdr's checks, including the ones about *meaning* rather than shape: an
//! empty `all`/`any`, an unknown agent status, a context compared against the wrong id
//! field, a value whose type does not match its field.
//!
//! Source and label are **normalized**, also as herdr does (trimmed; control characters
//! stripped from a label), so what the popup reads back is what herdr would have stored.

use serde_json::{Map, Value};

use super::envelope::CliError;
use super::{Ctx, Sub};

/// The global tmux option holding the active view. See the module docs for its contract.
pub const VIEW_OPTION: &str = "@agent_wizard_view";

// herdr's own limits, from `herdr-src/src/app/agent_view.rs`. Reproduced as constants so
// a drift shows up as a changed number rather than as a behaviour difference nobody
// notices until a plugin's filter is quietly ignored.
const MAX_FILTER_DEPTH: usize = 8;
const MAX_FILTER_NODES: usize = 64;
const MAX_FILTER_VALUES: usize = 32;
const MAX_SORT_FIELDS: usize = 8;
const MAX_SOURCE_CHARS: usize = 120;
const MAX_LABEL_CHARS: usize = 32;
const MAX_TOKEN_BYTES: usize = 32;

/// The fields a filter may name, from herdr's `AgentViewBuiltinField`. Anything else has
/// to be a `{"token": "<name>"}` — a pane metadata token, which is open-ended by design.
const BUILTIN_FIELDS: &[&str] = &[
    "status",
    "workspace_id",
    "tab_id",
    "pane_id",
    "agent",
    "seen",
    "state_change_seq",
];

/// The fields a sort may name, from herdr's `AgentViewBuiltinSortField`.
const BUILTIN_SORT_FIELDS: &[&str] = &[
    "workspace_order",
    "tab_order",
    "pane_order",
    "attention",
    "status",
    "agent",
    "seen",
    "state_change_seq",
];

/// The agent statuses a `status` filter may compare against — taw's five, which are
/// herdr's five.
const STATUSES: &[&str] = &["idle", "working", "blocked", "done", "unknown"];

/// The keys herdr's `AgentViewSetParams` has.
const PARAM_KEYS: &[&str] = &["source", "label", "filter", "sort"];

pub(crate) const SET: Sub = "view-set";
pub(crate) const CLEAR: Sub = "view-clear";

/// `agent view set` — validates, normalizes and stores the projection.
///
/// Either `params` (the whole herdr params object, for a caller that already has one) or
/// the individual pieces. clap makes the two forms mutually exclusive and requires
/// `--source` unless `--params` is given; `source` is then required *inside* the params
/// object, which is herdr's contract either way.
pub(crate) fn set(
    ctx: &Ctx,
    source: Option<&str>,
    label: Option<&str>,
    filter: Option<&str>,
    sort: Option<&str>,
    params: Option<&str>,
) -> anyhow::Result<Value> {
    let mut object = match params {
        Some(raw) => match parse(raw, "--params")? {
            Value::Object(map) => map,
            other => {
                return Err(CliError::invalid_argument(format!(
                    "--params must be a JSON object, got {other}"
                )));
            }
        },
        None => {
            let mut map = Map::new();
            // `source` is `Some` here: clap requires it unless `--params` was given.
            map.insert(
                "source".to_string(),
                Value::String(source.unwrap_or_default().to_string()),
            );
            if let Some(label) = label {
                map.insert("label".to_string(), Value::String(label.to_string()));
            }
            if let Some(filter) = filter {
                map.insert("filter".to_string(), parse(filter, "--filter")?);
            }
            if let Some(sort) = sort {
                map.insert("sort".to_string(), parse_sort(sort)?);
            }
            map
        }
    };

    // A key herdr does not have would be stored and then ignored, which reads as
    // "accepted" to the caller. Rejecting it names the typo instead.
    if let Some(unknown) = object
        .keys()
        .find(|key| !PARAM_KEYS.contains(&key.as_str()))
    {
        return Err(CliError::invalid_argument(format!(
            "unknown agent view field {unknown:?}; herdr's are {}",
            PARAM_KEYS.join(", ")
        )));
    }

    validate(&mut object)?;
    let object = Value::Object(object);
    let encoded = serde_json::to_string(&object)?;
    ctx.tmux
        .set_global_option(VIEW_OPTION, &encoded)
        .map_err(|e| CliError::tmux(format!("setting {VIEW_OPTION}: {e:#}")))?;
    Ok(reply(&object))
}

fn parse(raw: &str, flag: &str) -> anyhow::Result<Value> {
    serde_json::from_str(raw)
        .map_err(|e| CliError::invalid_argument(format!("{flag} is not valid JSON: {e}")))
}

/// `--sort` as herdr's `Vec<AgentViewSort>`. A single object is accepted as a one-element
/// list, because a one-field sort is the common case and `[{…}]` is easy to get wrong.
fn parse_sort(raw: &str) -> anyhow::Result<Value> {
    match parse(raw, "--sort")? {
        Value::Array(items) => Ok(Value::Array(items)),
        object @ Value::Object(_) => Ok(Value::Array(vec![object])),
        other => Err(CliError::invalid_argument(format!(
            "--sort must be a JSON array of sort objects (or one object), got {other}"
        ))),
    }
}

/// `agent view clear [--source <id>]` — unsets the option.
///
/// With `--source`, only a view that *this* source set is cleared. That is herdr's own
/// `AgentViewClearParams`, and it is what stops one plugin tearing down another's
/// projection when it tidies up after itself. Clearing a view someone else owns leaves it
/// alone and reports it still active, rather than erroring: the caller's intent — "my view
/// is gone" — already holds.
pub(crate) fn clear(ctx: &Ctx, source: Option<&str>) -> anyhow::Result<Value> {
    let Some(active) = read(&ctx.tmux) else {
        return Ok(reply(&Value::Null));
    };
    // Compared against the normalized form, since that is what was stored.
    if let Some(source) = source
        && active.get("source").and_then(Value::as_str) != Some(source.trim())
    {
        return Ok(reply(&active));
    }
    ctx.tmux
        .unset_global_option(VIEW_OPTION)
        .map_err(|e| CliError::tmux(format!("clearing {VIEW_OPTION}: {e:#}")))?;
    Ok(reply(&Value::Null))
}

/// The active view, or `None` when the option is unset or holds something that is not a
/// valid view.
///
/// Takes a `&Tmux` rather than the CLI's own context, because **the popup is the main
/// caller**: it runs as its own process (`taw ui`) and reads the projection from this
/// option every time it draws. [`set`] has already validated and normalized everything it
/// wrote, so the only way invalid content reaches here is someone setting the option by
/// hand.
///
/// Invalid content reads as "no view" rather than as an error: a projection nobody can
/// parse must not stop the agent list from being shown at all.
pub fn read(t: &crate::tmux::Tmux) -> Option<Value> {
    let raw = t
        .get_global_option(VIEW_OPTION)
        .unwrap_or_default()
        .filter(|v| !v.trim().is_empty())?;
    let mut object = match serde_json::from_str::<Value>(&raw).ok()? {
        Value::Object(map) => map,
        _ => return None,
    };
    if object.keys().any(|key| !PARAM_KEYS.contains(&key.as_str())) {
        return None;
    }
    validate(&mut object).ok()?;
    Some(Value::Object(object))
}

/// herdr's `agent_view` result: `{active, source?, label?}`.
fn reply(view: &Value) -> Value {
    let source = view.get("source").and_then(Value::as_str);
    serde_json::json!({
        "active": source.is_some(),
        "source": source,
        "label": view.get("label").and_then(Value::as_str),
    })
}

// ---------------------------------------------------------------------------
// Validation — herdr's `validate_agent_view`, rule for rule
// ---------------------------------------------------------------------------

/// Validates a params object in place, normalizing `source` and `label` as herdr does.
fn validate(object: &mut Map<String, Value>) -> anyhow::Result<()> {
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::invalid_argument("an agent view needs a string \"source\""))?;
    let source = normalize_source(source)?;
    object.insert("source".to_string(), Value::String(source));

    if let Some(label) = object.get("label") {
        let label = label
            .as_str()
            .ok_or_else(|| CliError::invalid_argument("an agent view label must be a string"))?;
        let label = normalize_label(label)?;
        object.insert("label".to_string(), Value::String(label));
    }

    if let Some(filter) = object.get("filter")
        && !filter.is_null()
    {
        let mut nodes = 0usize;
        validate_filter(filter, 1, &mut nodes)?;
    }

    if let Some(sort) = object.get("sort")
        && !sort.is_null()
    {
        let sorts = sort.as_array().ok_or_else(|| {
            CliError::invalid_argument("an agent view sort must be an array of sort objects")
        })?;
        if sorts.len() > MAX_SORT_FIELDS {
            return Err(CliError::invalid_argument(format!(
                "an agent view sort may name at most {MAX_SORT_FIELDS} fields, got {}",
                sorts.len()
            )));
        }
        for entry in sorts {
            validate_sort(entry)?;
        }
    }
    Ok(())
}

/// Trimmed, non-empty, at most 120 characters, `[A-Za-z0-9:._-]` — herdr's
/// `normalize_source`. The charset matters because a source is compared for equality by
/// `view clear --source`, and free text would make that comparison unreliable.
fn normalize_source(source: &str) -> anyhow::Result<String> {
    let source = source.trim();
    let ok = !source.is_empty()
        && source.chars().count() <= MAX_SOURCE_CHARS
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'));
    if !ok {
        return Err(CliError::invalid_argument(format!(
            "an agent view source must be non-empty, at most {MAX_SOURCE_CHARS} characters, \
             and hold only ASCII letters, digits, ':', '.', '_' or '-'"
        )));
    }
    Ok(source.to_string())
}

/// Trimmed, control characters removed, non-empty, at most 32 characters — herdr's
/// `normalize_label`. The label goes on one line of a popup, so a newline in it would
/// break the layout rather than say anything.
fn normalize_label(label: &str) -> anyhow::Result<String> {
    let label: String = label.trim().chars().filter(|c| !c.is_control()).collect();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(CliError::invalid_argument(format!(
            "an agent view label must be non-empty and at most {MAX_LABEL_CHARS} characters"
        )));
    }
    Ok(label)
}

/// One filter node. `depth` starts at 1 for the root, matching herdr; `nodes` accumulates
/// across the whole tree.
fn validate_filter(node: &Value, depth: usize, nodes: &mut usize) -> anyhow::Result<()> {
    if depth > MAX_FILTER_DEPTH {
        return Err(CliError::invalid_argument(format!(
            "an agent view filter may be nested at most {MAX_FILTER_DEPTH} levels deep"
        )));
    }
    *nodes += 1;
    if *nodes > MAX_FILTER_NODES {
        return Err(CliError::invalid_argument(format!(
            "an agent view filter may hold at most {MAX_FILTER_NODES} nodes"
        )));
    }
    let object = node
        .as_object()
        .ok_or_else(|| CliError::invalid_argument("every filter node must be a JSON object"))?;
    let op = object
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::invalid_argument("every filter node needs a string \"op\""))?;

    match op {
        "all" | "any" => {
            let filters = object
                .get("filters")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    CliError::invalid_argument(format!("\"op\":\"{op}\" needs a \"filters\" array"))
                })?;
            // Empty is rejected, not read as "match everything": herdr's evaluator makes
            // an empty `any` match *nothing*, so accepting one would store a filter that
            // silently hides every agent while this verb reported success.
            if filters.is_empty() {
                return Err(CliError::invalid_argument(
                    "an all/any filter must not be empty",
                ));
            }
            for child in filters {
                validate_filter(child, depth + 1, nodes)?;
            }
            Ok(())
        }
        "not" => {
            let inner = object.get("filter").ok_or_else(|| {
                CliError::invalid_argument("\"op\":\"not\" needs a single \"filter\"")
            })?;
            validate_filter(inner, depth + 1, nodes)
        }
        "eq" => {
            let value = object
                .get("value")
                .ok_or_else(|| CliError::invalid_argument("\"op\":\"eq\" needs a \"value\""))?;
            validate_field_value(object.get("field"), value)
        }
        "in" => {
            let values = object
                .get("values")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    CliError::invalid_argument("\"op\":\"in\" needs a \"values\" array")
                })?;
            if values.is_empty() || values.len() > MAX_FILTER_VALUES {
                return Err(CliError::invalid_argument(format!(
                    "an in filter needs 1 to {MAX_FILTER_VALUES} values, got {}",
                    values.len()
                )));
            }
            for value in values {
                validate_field_value(object.get("field"), value)?;
            }
            Ok(())
        }
        "exists" => validate_field(object.get("field")).map(|_| ()),
        other => Err(CliError::invalid_argument(format!(
            "unknown filter op {other:?}; herdr has all, any, not, eq, in, exists"
        ))),
    }
}

/// A field name: one of herdr's builtins, or a `{"token": "<name>"}`. Returns the builtin
/// name, or `None` for a token — which is what the value check needs to know.
fn validate_field(field: Option<&Value>) -> anyhow::Result<Option<&'static str>> {
    let field =
        field.ok_or_else(|| CliError::invalid_argument("this filter op needs a \"field\""))?;
    if let Some(name) = field.as_str() {
        return BUILTIN_FIELDS
            .iter()
            .find(|known| **known == name)
            .copied()
            .map(Some)
            .ok_or_else(|| {
                CliError::invalid_argument(format!(
                    "unknown filter field {name:?}; herdr's are {}, or {{\"token\": \"<name>\"}}",
                    BUILTIN_FIELDS.join(", ")
                ))
            });
    }
    match field.get("token").and_then(Value::as_str) {
        Some(token) => validate_token(token).map(|()| None),
        None => Err(CliError::invalid_argument(
            "a filter field must be a builtin name or {\"token\": \"<name>\"}",
        )),
    }
}

/// A field paired with the value it is compared against.
///
/// This is the check that catches a filter which is *shaped* right but cannot mean
/// anything: `seen` against a string, `status` against a status that does not exist,
/// `{"context":"current_tab_id"}` against `workspace_id`. herdr rejects all three, and its
/// evaluator would simply never match them.
fn validate_field_value(field: Option<&Value>, value: &Value) -> anyhow::Result<()> {
    let builtin = validate_field(field)?;
    // A context placeholder is only meaningful against the id field it names.
    if let Some(context) = value.get("context") {
        let context = context.as_str().ok_or_else(|| {
            CliError::invalid_argument("a filter value's \"context\" must be a string")
        })?;
        let wanted = match context {
            "current_workspace_id" => "workspace_id",
            "current_tab_id" => "tab_id",
            other => {
                return Err(CliError::invalid_argument(format!(
                    "unknown filter context {other:?}; herdr's are current_workspace_id, \
                     current_tab_id"
                )));
            }
        };
        if builtin != Some(wanted) {
            return Err(CliError::invalid_argument(format!(
                "{{\"context\":\"{context}\"}} can only be compared with the {wanted} field"
            )));
        }
        return Ok(());
    }

    match (builtin, value) {
        (Some("seen"), Value::Bool(_)) => Ok(()),
        (Some("state_change_seq"), Value::Number(n)) if n.is_u64() => Ok(()),
        (Some("state_change_seq"), Value::Number(n)) => Err(CliError::invalid_argument(format!(
            "state_change_seq must be compared with a non-negative integer, got {n}"
        ))),
        (Some("status"), Value::String(status)) => {
            if STATUSES.contains(&status.as_str()) {
                Ok(())
            } else {
                Err(CliError::invalid_argument(format!(
                    "unknown agent status {status:?}; taw's are {}",
                    STATUSES.join(", ")
                )))
            }
        }
        (Some("workspace_id" | "tab_id" | "pane_id" | "agent"), Value::String(_)) => Ok(()),
        // A metadata token holds text, whatever the token is.
        (None, Value::String(_)) => Ok(()),
        (Some(field), other) => Err(CliError::invalid_argument(format!(
            "the {field} field cannot be compared with {other}"
        ))),
        (None, other) => Err(CliError::invalid_argument(format!(
            "a token field cannot be compared with {other}; tokens hold text"
        ))),
    }
}

/// One `{"field": …, "order": "asc"|"desc"}` sort entry.
fn validate_sort(entry: &Value) -> anyhow::Result<()> {
    let object = entry
        .as_object()
        .ok_or_else(|| CliError::invalid_argument("every sort entry must be a JSON object"))?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "field" | "order"))
    {
        return Err(CliError::invalid_argument(format!(
            "unknown sort key {unknown:?}; herdr's are field, order"
        )));
    }
    let field = object
        .get("field")
        .ok_or_else(|| CliError::invalid_argument("every sort entry needs a \"field\""))?;
    if let Some(name) = field.as_str() {
        if !BUILTIN_SORT_FIELDS.contains(&name) {
            return Err(CliError::invalid_argument(format!(
                "unknown sort field {name:?}; herdr's are {}, or {{\"token\": \"<name>\"}}",
                BUILTIN_SORT_FIELDS.join(", ")
            )));
        }
    } else {
        match field.get("token").and_then(Value::as_str) {
            Some(token) => validate_token(token)?,
            None => {
                return Err(CliError::invalid_argument(
                    "a sort field must be a builtin name or {\"token\": \"<name>\"}",
                ));
            }
        }
    }
    // `order` defaults to `asc` when absent, as herdr's schema does.
    if let Some(order) = object.get("order")
        && !matches!(order.as_str(), Some("asc" | "desc"))
    {
        return Err(CliError::invalid_argument(format!(
            "a sort order must be \"asc\" or \"desc\", got {order}"
        )));
    }
    Ok(())
}

/// A metadata token name: non-empty, at most 32 bytes, `[A-Za-z0-9_-]` — herdr's
/// `validate_token`, and the same charset a pane token must satisfy to be *written*
/// (`pane report-metadata --token`), so a filter cannot name one that could not exist.
fn validate_token(token: &str) -> anyhow::Result<()> {
    let ok = !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(CliError::invalid_argument(format!(
            "invalid agent view token {token:?}: use 1 to {MAX_TOKEN_BYTES} letters, digits, \
             '_' or '-'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::envelope::code_for;

    fn params(json: &str) -> Map<String, Value> {
        match serde_json::from_str::<Value>(json).unwrap() {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn accepted(json: &str) -> Map<String, Value> {
        let mut object = params(json);
        validate(&mut object).unwrap_or_else(|e| panic!("{json} should be valid: {e}"));
        object
    }

    /// Asserts `json` is refused as an `invalid_argument`, returning the message so a
    /// caller can check it names what was wrong.
    fn rejected(json: &str) -> String {
        let mut object = params(json);
        match validate(&mut object) {
            Ok(()) => panic!("{json} should have been rejected"),
            Err(err) => {
                assert_eq!(code_for(&err), "invalid_argument", "for {json}");
                err.to_string()
            }
        }
    }

    fn with_filter(filter: &str) -> String {
        format!(r#"{{"source":"s","filter":{filter}}}"#)
    }

    #[test]
    fn every_leaf_op_and_field_shape_herdr_documents_is_accepted() {
        for filter in [
            r#"{"op":"eq","field":"status","value":"blocked"}"#,
            r#"{"op":"eq","field":"seen","value":false}"#,
            r#"{"op":"eq","field":"state_change_seq","value":42}"#,
            r#"{"op":"eq","field":{"token":"ticket"},"value":"ABC-1"}"#,
            r#"{"op":"in","field":"status","values":["blocked","done"]}"#,
            r#"{"op":"exists","field":{"token":"ticket"}}"#,
            r#"{"op":"eq","field":"workspace_id","value":{"context":"current_workspace_id"}}"#,
            r#"{"op":"eq","field":"tab_id","value":{"context":"current_tab_id"}}"#,
        ] {
            accepted(&with_filter(filter));
        }
        for field in BUILTIN_FIELDS {
            accepted(&with_filter(&format!(
                r#"{{"op":"exists","field":"{field}"}}"#
            )));
        }
        for status in STATUSES {
            accepted(&with_filter(&format!(
                r#"{{"op":"eq","field":"status","value":"{status}"}}"#
            )));
        }
    }

    #[test]
    fn the_combinators_nest_each_other() {
        accepted(&with_filter(
            r#"{"op":"all","filters":[
                {"op":"eq","field":"status","value":"blocked"},
                {"op":"not","filter":{"op":"eq","field":"seen","value":true}}
              ]}"#,
        ));
        accepted(&with_filter(
            r#"{"op":"not","filter":{"op":"any","filters":[
                {"op":"exists","field":"agent"}
              ]}}"#,
        ));
    }

    #[test]
    fn an_empty_all_or_any_is_rejected_rather_than_hiding_every_agent() {
        // herdr's evaluator makes an empty `any` match *nothing*, so storing one would
        // blank the panel while this verb reported success.
        assert!(
            rejected(&with_filter(r#"{"op":"any","filters":[]}"#)).contains("must not be empty")
        );
        assert!(
            rejected(&with_filter(r#"{"op":"all","filters":[]}"#)).contains("must not be empty")
        );
        // Same reasoning for an `in` with nothing to match against.
        assert!(
            rejected(&with_filter(r#"{"op":"in","field":"agent","values":[]}"#))
                .contains("1 to 32 values")
        );
    }

    #[test]
    fn a_status_that_does_not_exist_is_rejected() {
        // `busy` is not one of taw's five states, so it could never equal a real one.
        let message = rejected(&with_filter(
            r#"{"op":"eq","field":"status","value":"busy"}"#,
        ));
        assert!(message.contains("busy"), "{message}");
        assert!(
            rejected(&with_filter(
                r#"{"op":"in","field":"status","values":["idle","busy"]}"#
            ))
            .contains("busy")
        );
    }

    #[test]
    fn a_context_may_only_be_compared_with_the_id_field_it_names() {
        assert!(
            rejected(&with_filter(
                r#"{"op":"eq","field":"tab_id","value":{"context":"current_workspace_id"}}"#
            ))
            .contains("workspace_id")
        );
        rejected(&with_filter(
            r#"{"op":"eq","field":"agent","value":{"context":"current_tab_id"}}"#,
        ));
        rejected(&with_filter(
            r#"{"op":"eq","field":{"token":"t"},"value":{"context":"current_tab_id"}}"#,
        ));
        assert!(
            rejected(&with_filter(
                r#"{"op":"eq","field":"tab_id","value":{"context":"whatever"}}"#
            ))
            .contains("whatever")
        );
    }

    #[test]
    fn a_value_whose_type_does_not_match_its_field_is_rejected() {
        // Each of these is shaped correctly and could never match anything.
        for filter in [
            r#"{"op":"eq","field":"seen","value":"true"}"#,
            r#"{"op":"eq","field":"status","value":true}"#,
            r#"{"op":"eq","field":"state_change_seq","value":"42"}"#,
            r#"{"op":"eq","field":"state_change_seq","value":-1}"#,
            r#"{"op":"eq","field":"state_change_seq","value":1.5}"#,
            r#"{"op":"eq","field":"agent","value":7}"#,
            r#"{"op":"eq","field":{"token":"t"},"value":7}"#,
            r#"{"op":"eq","field":"status","value":null}"#,
            r#"{"op":"eq","field":"status","value":["a"]}"#,
        ] {
            rejected(&with_filter(filter));
        }
    }

    #[test]
    fn an_unknown_op_or_field_or_a_malformed_node_is_rejected_by_name() {
        assert!(rejected(&with_filter(r#"{"op":"between","field":"status"}"#)).contains("between"));
        assert!(
            rejected(&with_filter(
                r#"{"op":"eq","field":"colour","value":"red"}"#
            ))
            .contains("colour")
        );
        for filter in [
            r#"{"field":"status","value":"idle"}"#,
            r#"{"op":"all","filters":{"op":"exists","field":"agent"}}"#,
            r#"{"op":"not"}"#,
            r#"{"op":"eq","field":"status"}"#,
            r#"{"op":"in","field":"status"}"#,
            r#"{"op":"exists"}"#,
            r#"{"op":"eq","field":{"nope":"x"},"value":"x"}"#,
            r#""not an object""#,
        ] {
            rejected(&with_filter(filter));
        }
    }

    #[test]
    fn a_token_must_satisfy_the_charset_a_pane_token_is_written_with() {
        accepted(&with_filter(
            r#"{"op":"exists","field":{"token":"ticket-1_A"}}"#,
        ));
        for bad in ["", "has space", "semi;colon", "uni\u{e9}"] {
            let filter = format!(
                r#"{{"op":"exists","field":{{"token":{}}}}}"#,
                serde_json::to_string(bad).unwrap()
            );
            rejected(&with_filter(&filter));
        }
        let long = "x".repeat(MAX_TOKEN_BYTES + 1);
        rejected(&with_filter(&format!(
            r#"{{"op":"exists","field":{{"token":"{long}"}}}}"#
        )));
        accepted(&with_filter(&format!(
            r#"{{"op":"exists","field":{{"token":"{}"}}}}"#,
            "x".repeat(MAX_TOKEN_BYTES)
        )));
    }

    #[test]
    fn the_filter_depth_and_node_caps_are_herdrs_eight_and_sixty_four() {
        // Depth: the root counts as level 1, so seven `not` wrappers is the limit.
        let mut at_limit = r#"{"op":"exists","field":"agent"}"#.to_string();
        for _ in 0..(MAX_FILTER_DEPTH - 1) {
            at_limit = format!(r#"{{"op":"not","filter":{at_limit}}}"#);
        }
        accepted(&with_filter(&at_limit));
        let too_deep = format!(r#"{{"op":"not","filter":{at_limit}}}"#);
        assert!(rejected(&with_filter(&too_deep)).contains("8 levels"));

        // Nodes: one `any` plus its children.
        let child = r#"{"op":"exists","field":"agent"}"#;
        let children = vec![child; MAX_FILTER_NODES - 1].join(",");
        accepted(&with_filter(&format!(
            r#"{{"op":"any","filters":[{children}]}}"#
        )));
        let too_many = vec![child; MAX_FILTER_NODES].join(",");
        assert!(
            rejected(&with_filter(&format!(
                r#"{{"op":"any","filters":[{too_many}]}}"#
            )))
            .contains("64 nodes")
        );
    }

    #[test]
    fn an_in_filter_takes_at_most_thirty_two_values() {
        let values = vec![r#""a""#; MAX_FILTER_VALUES].join(",");
        accepted(&with_filter(&format!(
            r#"{{"op":"in","field":"agent","values":[{values}]}}"#
        )));
        let too_many = vec![r#""a""#; MAX_FILTER_VALUES + 1].join(",");
        rejected(&with_filter(&format!(
            r#"{{"op":"in","field":"agent","values":[{too_many}]}}"#
        )));
    }

    #[test]
    fn source_is_normalized_and_its_charset_enforced() {
        // Trimmed, and the trimmed form is what gets stored — herdr's normalize_source.
        let object = accepted(r#"{"source":"  review:1.0_a-b  "}"#);
        assert_eq!(object["source"], "review:1.0_a-b");

        for bad in ["", "   ", "has space", "star*", "sl/ash", "uni\u{e9}"] {
            rejected(&format!(
                r#"{{"source":{}}}"#,
                serde_json::to_string(bad).unwrap()
            ));
        }
        accepted(&format!(
            r#"{{"source":"{}"}}"#,
            "x".repeat(MAX_SOURCE_CHARS)
        ));
        rejected(&format!(
            r#"{{"source":"{}"}}"#,
            "x".repeat(MAX_SOURCE_CHARS + 1)
        ));
        // A params object with no source at all is not a view.
        rejected(r#"{"label":"no source"}"#);
        rejected(r#"{"source":7}"#);
    }

    #[test]
    fn label_is_normalized_with_control_characters_stripped() {
        let object = accepted("{\"source\":\"s\",\"label\":\"  Review\\tqueue  \"}");
        assert_eq!(
            object["label"], "Reviewqueue",
            "control characters are removed, as herdr does"
        );
        rejected(r#"{"source":"s","label":""}"#);
        rejected("{\"source\":\"s\",\"label\":\"\\n\"}");
        rejected(r#"{"source":"s","label":7}"#);
        accepted(&format!(
            r#"{{"source":"s","label":"{}"}}"#,
            "x".repeat(MAX_LABEL_CHARS)
        ));
        rejected(&format!(
            r#"{{"source":"s","label":"{}"}}"#,
            "x".repeat(MAX_LABEL_CHARS + 1)
        ));
    }

    #[test]
    fn sort_accepts_herdrs_fields_and_orders_and_caps_at_eight() {
        for field in BUILTIN_SORT_FIELDS {
            accepted(&format!(
                r#"{{"source":"s","sort":[{{"field":"{field}"}}]}}"#
            ));
        }
        accepted(r#"{"source":"s","sort":[{"field":"attention","order":"desc"}]}"#);
        accepted(r#"{"source":"s","sort":[{"field":{"token":"ticket"},"order":"asc"}]}"#);
        // An empty sort is legal: it means "leave the panel's own order alone".
        accepted(r#"{"source":"s","sort":[]}"#);

        assert!(rejected(r#"{"source":"s","sort":[{"field":"whenever"}]}"#).contains("whenever"));
        rejected(r#"{"source":"s","sort":[{"field":"attention","order":"sideways"}]}"#);
        rejected(r#"{"source":"s","sort":[{"order":"asc"}]}"#);
        rejected(r#"{"source":"s","sort":[{"field":"attention","reverse":true}]}"#);
        rejected(r#"{"source":"s","sort":{"field":"attention"}}"#);
        rejected(r#"{"source":"s","sort":["attention"]}"#);

        let entry = r#"{"field":"attention"}"#;
        accepted(&format!(
            r#"{{"source":"s","sort":[{}]}}"#,
            [entry; MAX_SORT_FIELDS].join(",")
        ));
        assert!(
            rejected(&format!(
                r#"{{"source":"s","sort":[{}]}}"#,
                [entry; MAX_SORT_FIELDS + 1].join(",")
            ))
            .contains("8 fields")
        );
    }

    #[test]
    fn a_null_filter_or_sort_is_the_same_as_omitting_it() {
        // herdr's params hold `Option<AgentViewFilter>`, so a client that serializes its
        // `None` as `null` has sent a legal document.
        accepted(r#"{"source":"s","filter":null,"sort":null}"#);
    }

    #[test]
    fn parse_sort_accepts_one_object_as_a_single_entry_list() {
        assert_eq!(
            parse_sort(r#"{"field":"attention"}"#).unwrap(),
            serde_json::json!([{"field": "attention"}])
        );
        assert_eq!(
            parse_sort(r#"[{"field":"attention"}]"#).unwrap(),
            serde_json::json!([{"field": "attention"}])
        );
        assert_eq!(code_for(&parse_sort("7").unwrap_err()), "invalid_argument");
        assert_eq!(
            code_for(&parse_sort("not json").unwrap_err()),
            "invalid_argument"
        );
    }
}
