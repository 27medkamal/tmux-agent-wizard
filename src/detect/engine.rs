// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Manifest parsing, rule compilation and evaluation.
//!
//! Rule semantics are herdr's, verbatim: `contains` is a case-insensitive substring,
//! `regex` runs against the whole region, `line_regex` matches when any line of the
//! region matches, every listed pattern of a kind must match, and `any`/`all`/`not`
//! are nested gates of the same three matchers. The first rule with the highest
//! priority wins.

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::detect::{
    AgentState, MANIFEST_ENGINE_VERSION, Snapshot,
    regions::{self, region_text},
    update::ManifestVersion,
};

const MAX_RULES_PER_MANIFEST: usize = 128;
const MAX_GATE_DEPTH: usize = 8;
const MAX_TOTAL_GATES: usize = 512;
const MAX_MATCHERS_PER_GATE: usize = 32;
const MAX_TOTAL_MATCHERS: usize = 1024;
const MAX_MATCHER_CHARS: usize = 512;
const MAX_PREVIEW_CHARS: usize = 240;

/// Where the manifest that produced an evaluation came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestSource {
    Bundled,
    Remote,
    Override,
}

/// Where one *rule* came from, independent of [`ManifestSource`]: the manifest that
/// carried it (bundled, remote or override — whichever the loader picked), or a taw
/// [`supplements`](crate::detect::supplements) rule appended after that manifest was
/// chosen. herdr has no equivalent — every rule herdr ever evaluates is `Manifest`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleSource {
    #[default]
    Manifest,
    Taw,
}

/// Why a manifest could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to parse manifest TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

impl ManifestError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

/// A parsed, validated and compiled manifest, ready to evaluate.
#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub id: String,
    pub version: Option<String>,
    pub source: ManifestSource,
    /// Set by the loader when a shadowed manifest was ignored, e.g. a broken
    /// override. Surfaced in every [`Evaluation`] made with this manifest.
    pub warning: Option<String>,
    aliases: Vec<String>,
    min_engine_version: Option<u32>,
    rules: Vec<ManifestRule>,
    compiled: Vec<CompiledRule>,
}

impl LoadedManifest {
    /// True when this manifest claims the given agent id, by `id` or by alias.
    pub(crate) fn matches_id(&self, id: &str) -> bool {
        self.id == id || self.aliases.iter().any(|alias| alias == id)
    }

    /// The declared version as an ordered value. `None` when undeclared.
    pub(crate) fn ordered_version(&self) -> Option<ManifestVersion> {
        self.version
            .as_deref()
            .and_then(|version| ManifestVersion::parse(version).ok())
    }

    /// The declared `min_engine_version`, which remote manifests must carry.
    pub(crate) fn min_engine_version(&self) -> Option<u32> {
        self.min_engine_version
    }

    /// Whether a rule of this manifest, a supplement's included, reads the agent idle off its
    /// screen: a region other than the terminal's title or progress. One with none says it is idle
    /// only through what it sets there, or only through the known-agent fallback, a frame no rule
    /// matches.
    pub fn has_screen_idle_rule(&self) -> bool {
        self.rules.iter().any(|rule| {
            rule.state() == AgentState::Idle
                && !matches!(rule.region.trim(), "osc_title" | "osc_progress")
        })
    }

    /// Parse a taw [`supplements`](crate::detect::supplements) document — TOML holding
    /// only `[[rules]]`, no manifest header — and append its rules to this manifest,
    /// tagged [`RuleSource::Taw`]. Returns how many rules were appended.
    ///
    /// Reuses [`validate_manifest`] and [`compile_manifest`] against a manifest header
    /// borrowed from `self`, so a supplement's rules are held to exactly the same
    /// limits and region/regex checks as any vendored manifest's. Anything beyond a
    /// bare `rules` array — an `id`, a `version`, anything a real manifest could
    /// declare — is rejected: a supplement is not a manifest.
    pub fn append_supplement(&mut self, toml_text: &str) -> Result<usize, ManifestError> {
        let doc: SupplementDocument = toml::from_str(toml_text)?;
        let synthetic = AgentManifest {
            id: self.id.clone(),
            version: None,
            min_engine_version: None,
            _updated_at: None,
            aliases: Vec::new(),
            rules: doc.rules,
        };
        validate_manifest(&synthetic)?;
        if self.rules.len() + synthetic.rules.len() > MAX_RULES_PER_MANIFEST {
            return Err(ManifestError::invalid(format!(
                "manifest plus supplement would contain {} rules, max is {MAX_RULES_PER_MANIFEST}",
                self.rules.len() + synthetic.rules.len()
            )));
        }
        let compiled = compile_manifest(&synthetic)?;
        let count = synthetic.rules.len();

        self.rules
            .extend(synthetic.rules.into_iter().map(|mut rule| {
                rule.source = RuleSource::Taw;
                rule
            }));
        self.compiled.extend(compiled);
        Ok(count)
    }
}

/// A taw supplement document: only `[[rules]]`, nothing a manifest header would carry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupplementDocument {
    #[serde(default)]
    rules: Vec<ManifestRule>,
}

/// The rule an [`Evaluation`] took its state from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MatchedRule {
    pub id: String,
    pub priority: i32,
    pub region: String,
    pub state: AgentState,
    pub source: RuleSource,
}

/// One rule's outcome, kept for `taw agent explain`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EvaluatedRule {
    pub id: String,
    pub region: String,
    pub state: AgentState,
    pub matched: bool,
    pub priority: i32,
    pub evidence: RuleEvidence,
    pub source: RuleSource,
}

/// What a rule looked for, and what it looked at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RuleEvidence {
    pub contains: Vec<String>,
    pub regex: Vec<String>,
    pub line_regex: Vec<String>,
    pub all_count: usize,
    pub any_count: usize,
    pub not_count: usize,
    pub region_bytes: usize,
    pub region_preview: String,
}

/// The result of matching a snapshot against a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evaluation {
    pub agent: String,
    pub state: AgentState,
    pub matched_rule: Option<MatchedRule>,
    pub evaluated_rules: Vec<EvaluatedRule>,
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    pub skip_state_update: bool,
    pub manifest_version: Option<String>,
    pub source: ManifestSource,
    pub warning: Option<String>,
}

/// Parse, validate and compile a manifest.
///
/// Rejects unknown TOML fields, empty rule sets, unknown region names, invalid
/// regexes, over-complex rule trees and manifests that need a newer engine than
/// [`MANIFEST_ENGINE_VERSION`].
pub fn parse_manifest(
    toml_text: &str,
    source: ManifestSource,
) -> Result<LoadedManifest, ManifestError> {
    let manifest: AgentManifest = toml::from_str(toml_text)?;
    validate_manifest(&manifest)?;
    let compiled = compile_manifest(&manifest)?;

    Ok(LoadedManifest {
        id: manifest.id,
        version: manifest.version.map(|version| version.to_string()),
        source,
        warning: None,
        aliases: manifest.aliases,
        min_engine_version: manifest.min_engine_version,
        rules: manifest.rules,
        compiled,
    })
}

/// Match a snapshot against every rule of a manifest.
///
/// The first rule with the highest priority wins; a later rule replaces it only
/// when its priority is strictly higher. With no match the verdict is `Idle` with
/// no matched rule — herdr's known-agent fallback.
pub fn evaluate(manifest: &LoadedManifest, snap: &Snapshot) -> Evaluation {
    let mut matched: Option<&ManifestRule> = None;
    let mut evaluated_rules = Vec::with_capacity(manifest.rules.len());

    for (rule, compiled) in manifest.rules.iter().zip(&manifest.compiled) {
        let region = region_text(&rule.region, snap).unwrap_or("");
        let rule_matched = compiled_rule_matches(compiled, region);
        evaluated_rules.push(EvaluatedRule {
            id: rule.id.clone(),
            region: rule.region.clone(),
            state: rule.state(),
            matched: rule_matched,
            priority: rule.priority,
            evidence: rule_evidence(rule, region),
            source: rule.source,
        });

        if !rule_matched {
            continue;
        }
        match matched {
            Some(previous) if previous.priority >= rule.priority => {}
            _ => matched = Some(rule),
        }
    }

    let Some(rule) = matched else {
        return Evaluation {
            agent: manifest.id.clone(),
            state: AgentState::Idle,
            matched_rule: None,
            evaluated_rules,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
            manifest_version: manifest.version.clone(),
            source: manifest.source,
            warning: manifest.warning.clone(),
        };
    };

    let state = rule.state();
    Evaluation {
        agent: manifest.id.clone(),
        state,
        matched_rule: Some(MatchedRule {
            id: rule.id.clone(),
            priority: rule.priority,
            region: rule.region.clone(),
            state,
            source: rule.source,
        }),
        evaluated_rules,
        visible_idle: rule.visible_idle && state == AgentState::Idle,
        visible_blocker: rule.visible_blocker && state == AgentState::Blocked,
        visible_working: rule.visible_working && state == AgentState::Working,
        skip_state_update: rule.skip_state_update,
        manifest_version: manifest.version.clone(),
        source: manifest.source,
        warning: manifest.warning.clone(),
    }
}

/// The `id` and `aliases` of a manifest, without compiling its rules.
pub(crate) fn manifest_identity(toml_text: &str) -> Option<(String, Vec<String>)> {
    let header: ManifestHeader = toml::from_str(toml_text).ok()?;
    Some((header.id, header.aliases))
}

/// The declared `version` of a manifest, without compiling its rules.
pub(crate) fn manifest_version(toml_text: &str) -> Option<String> {
    let header: ManifestHeader = toml::from_str(toml_text).ok()?;
    Some(header.version?.to_string())
}

// ---------------------------------------------------------------------------
// Manifest format
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct AgentManifest {
    id: String,
    version: Option<ManifestVersion>,
    min_engine_version: Option<u32>,
    #[serde(rename = "updated_at")]
    _updated_at: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    rules: Vec<ManifestRule>,
}

/// The header fields alone, for lookups that must not pay to compile the rules.
#[derive(Debug, Deserialize)]
struct ManifestHeader {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    version: Option<ManifestVersion>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestRule {
    id: String,
    state: Option<ManifestState>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    #[serde(default)]
    visible_working: bool,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
    /// Never a TOML field: no manifest or supplement file declares its own rules'
    /// source, it is stamped by whichever code path produced the rule —
    /// [`parse_manifest`] leaves it at the `Manifest` default,
    /// [`LoadedManifest::append_supplement`] overwrites it to `Taw`.
    #[serde(skip)]
    source: RuleSource,
}

impl ManifestRule {
    fn state(&self) -> AgentState {
        self.state
            .map(AgentState::from)
            .unwrap_or(AgentState::Unknown)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestGate {
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl From<ManifestState> for AgentState {
    fn from(value: ManifestState) -> Self {
        match value {
            ManifestState::Idle => Self::Idle,
            ManifestState::Working => Self::Working,
            ManifestState::Blocked => Self::Blocked,
            ManifestState::Unknown => Self::Unknown,
        }
    }
}

fn default_region() -> String {
    "whole_recent".to_string()
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ManifestComplexity {
    total_gates: usize,
    total_matchers: usize,
}

fn validate_manifest(manifest: &AgentManifest) -> Result<(), ManifestError> {
    if let Some(required) = manifest.min_engine_version
        && required > MANIFEST_ENGINE_VERSION
    {
        return Err(ManifestError::invalid(format!(
            "manifest requires engine {required}, current engine is {MANIFEST_ENGINE_VERSION}"
        )));
    }
    if manifest.rules.is_empty() {
        return Err(ManifestError::invalid(
            "manifest must contain at least one rule",
        ));
    }
    if manifest.rules.len() > MAX_RULES_PER_MANIFEST {
        return Err(ManifestError::invalid(format!(
            "manifest contains {} rules, max is {MAX_RULES_PER_MANIFEST}",
            manifest.rules.len()
        )));
    }

    let mut complexity = ManifestComplexity::default();
    for rule in &manifest.rules {
        if rule.id.trim().is_empty() {
            return Err(ManifestError::invalid("manifest rule id must not be empty"));
        }
        if rule.skip_state_update {
            if rule.state != Some(ManifestState::Unknown) {
                return Err(ManifestError::invalid(format!(
                    "rule {} uses skip_state_update without state = \"unknown\"",
                    rule.id
                )));
            }
            if rule.visible_idle || rule.visible_blocker || rule.visible_working {
                return Err(ManifestError::invalid(format!(
                    "rule {} uses skip_state_update with visible state evidence",
                    rule.id
                )));
            }
        }
        regions::validate_region_name(&rule.region).map_err(|err| {
            ManifestError::invalid(format!("rule {} uses invalid region: {err}", rule.id))
        })?;
        if let Some(required) = regions::min_engine_version_for_region(&rule.region)
            && manifest
                .min_engine_version
                .is_some_and(|declared| declared < required)
        {
            return Err(ManifestError::invalid(format!(
                "rule {} uses {} but min_engine_version is below {required}",
                rule.id,
                rule.region.trim()
            )));
        }
        validate_gate(&gate_from_rule(rule), "rule", 0, &mut complexity).map_err(|err| {
            ManifestError::invalid(format!("rule {} has invalid matcher gates: {err}", rule.id))
        })?;
    }

    Ok(())
}

fn validate_gate(
    gate: &ManifestGate,
    context: &str,
    depth: usize,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("{context} exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    validate_matcher_limits(gate, context, complexity)?;
    if !gate_has_positive_matcher(gate) {
        return Err(format!("{context} must contain a positive matcher"));
    }
    validate_regex_patterns(&gate.regex, context, "regex")?;
    validate_regex_patterns(&gate.line_regex, context, "line_regex")?;
    for nested in &gate.all {
        validate_gate(nested, "all gate", depth + 1, complexity)?;
    }
    for nested in &gate.any {
        validate_gate(nested, "any gate", depth + 1, complexity)?;
    }
    for nested in &gate.not_gate {
        if !gate_has_any_matcher(nested) {
            return Err(format!("{context} contains an empty not gate"));
        }
        validate_not_gate(nested, depth + 1, complexity)?;
    }
    Ok(())
}

fn validate_not_gate(
    gate: &ManifestGate,
    depth: usize,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("not gate exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    validate_matcher_limits(gate, "not gate", complexity)?;
    if !gate_has_any_matcher(gate) {
        return Err("not gate must contain a matcher".to_string());
    }
    validate_regex_patterns(&gate.regex, "not gate", "regex")?;
    validate_regex_patterns(&gate.line_regex, "not gate", "line_regex")?;
    for nested in &gate.all {
        validate_gate(nested, "not all gate", depth + 1, complexity)?;
    }
    for nested in &gate.any {
        validate_gate(nested, "not any gate", depth + 1, complexity)?;
    }
    for nested in &gate.not_gate {
        validate_not_gate(nested, depth + 1, complexity)?;
    }
    Ok(())
}

fn validate_matcher_limits(
    gate: &ManifestGate,
    context: &str,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    let matcher_count = gate.contains.len() + gate.regex.len() + gate.line_regex.len();
    if matcher_count > MAX_MATCHERS_PER_GATE {
        return Err(format!(
            "{context} has {matcher_count} direct matchers, max is {MAX_MATCHERS_PER_GATE}"
        ));
    }
    complexity.total_matchers += matcher_count;
    if complexity.total_matchers > MAX_TOTAL_MATCHERS {
        return Err(format!(
            "manifest exceeds max matcher count {MAX_TOTAL_MATCHERS}"
        ));
    }
    for value in gate
        .contains
        .iter()
        .chain(gate.regex.iter())
        .chain(gate.line_regex.iter())
    {
        if value.chars().count() > MAX_MATCHER_CHARS {
            return Err(format!(
                "{context} matcher exceeds max length {MAX_MATCHER_CHARS}"
            ));
        }
    }
    Ok(())
}

fn validate_regex_patterns(patterns: &[String], context: &str, field: &str) -> Result<(), String> {
    for pattern in patterns {
        Regex::new(pattern).map_err(|err| {
            format!("{context} contains invalid {field} pattern {pattern:?}: {err}")
        })?;
    }
    Ok(())
}

fn gate_has_positive_matcher(gate: &ManifestGate) -> bool {
    !gate.contains.is_empty()
        || !gate.regex.is_empty()
        || !gate.line_regex.is_empty()
        || !gate.all.is_empty()
        || !gate.any.is_empty()
}

fn gate_has_any_matcher(gate: &ManifestGate) -> bool {
    gate_has_positive_matcher(gate) || !gate.not_gate.is_empty()
}

// ---------------------------------------------------------------------------
// Compilation and matching
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CompiledRule {
    gate: CompiledGate,
}

#[derive(Debug, Clone)]
struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
    /// Lowercased once at compile time; the region is lowercased once per rule.
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

fn gate_from_rule(rule: &ManifestRule) -> ManifestGate {
    ManifestGate {
        all: rule.all.clone(),
        any: rule.any.clone(),
        not_gate: rule.not_gate.clone(),
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
    }
}

fn compile_manifest(manifest: &AgentManifest) -> Result<Vec<CompiledRule>, ManifestError> {
    manifest
        .rules
        .iter()
        .map(|rule| {
            compile_gate(&gate_from_rule(rule))
                .map(|gate| CompiledRule { gate })
                .map_err(|err| {
                    ManifestError::invalid(format!("rule {} could not be compiled: {err}", rule.id))
                })
        })
        .collect()
}

fn compile_gate(gate: &ManifestGate) -> Result<CompiledGate, String> {
    Ok(CompiledGate {
        all: gate
            .all
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        any: gate
            .any
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        not_gate: gate
            .not_gate
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        contains: gate.contains.iter().map(|n| n.to_lowercase()).collect(),
        regex: gate
            .regex
            .iter()
            .map(|pattern| Regex::new(pattern).map_err(|err| err.to_string()))
            .collect::<Result<_, _>>()?,
        line_regex: gate
            .line_regex
            .iter()
            .map(|pattern| Regex::new(pattern).map_err(|err| err.to_string()))
            .collect::<Result<_, _>>()?,
    })
}

fn compiled_rule_matches(rule: &CompiledRule, text: &str) -> bool {
    let lower_text = text.to_lowercase();
    compiled_gate_matches(&rule.gate, text, &lower_text)
}

fn compiled_gate_matches(gate: &CompiledGate, text: &str, lower_text: &str) -> bool {
    if !gate
        .contains
        .iter()
        .all(|needle| lower_text.contains(needle))
    {
        return false;
    }
    if !gate.regex.iter().all(|regex| regex.is_match(text)) {
        return false;
    }
    if !gate
        .line_regex
        .iter()
        .all(|regex| text.lines().any(|line| regex.is_match(line)))
    {
        return false;
    }
    if !gate
        .all
        .iter()
        .all(|nested| compiled_gate_matches(nested, text, lower_text))
    {
        return false;
    }
    if !gate.any.is_empty()
        && !gate
            .any
            .iter()
            .any(|nested| compiled_gate_matches(nested, text, lower_text))
    {
        return false;
    }
    if gate
        .not_gate
        .iter()
        .any(|nested| compiled_gate_matches(nested, text, lower_text))
    {
        return false;
    }
    true
}

fn rule_evidence(rule: &ManifestRule, region: &str) -> RuleEvidence {
    RuleEvidence {
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
        all_count: rule.all.len(),
        any_count: rule.any.len(),
        not_count: rule.not_gate.len(),
        region_bytes: region.len(),
        region_preview: bounded_preview(region),
    }
}

fn bounded_preview(text: &str) -> String {
    let mut preview: String = text.chars().take(MAX_PREVIEW_CHARS).collect();
    if text.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
}

// Follows herdr's src/detect/manifest/tests.rs. Differences:
// adapted to taw types: `explain(Agent::X, screen)` becomes `evaluate` over the
// bundled manifest for that id, and herdr's `fallback_reason` assertions become
// `matched_rule.is_none()`. The loader and cache tests live in loader.rs, which also
// holds taw's own tests for what herdr's three tests of its process-wide manifest
// cache pin (a reload, and the fallback verdict keeping the active version); the
// shared compiled rules of that cache have no counterpart here. The Codex tests run
// against herdr master's manifest (2026.10.01.1), whose `osc_title_idle` rule is back,
// so they expect that rule where herdr does. They did not for a while: the manifest
// 2026.09.23.1 that herdr 0.9.2 and 0.9.3 shipped had dropped the rule, and the tests
// that asserted it, two of them under the names
// `codex_osc_title_plain_is_idle` and
// `codex_background_terminal_screen_does_not_override_osc_idle`, expected the
// known-agent fallback. The Grok tests are from master (7df919d00e); the Cline, Pi,
// Kiro and later Codex tests are taw's.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::identity;

    fn bundled(id: &str) -> LoadedManifest {
        let text = identity::bundled_manifest_text(id)
            .unwrap_or_else(|| panic!("missing bundled manifest for {id}"));
        parse_manifest(text, ManifestSource::Bundled)
            .unwrap_or_else(|err| panic!("bundled {id} manifest is invalid: {err}"))
    }

    fn explain(id: &str, screen: &str) -> Evaluation {
        osc_explain(id, screen, "", "")
    }

    fn osc_explain(id: &str, screen: &str, osc_title: &str, osc_progress: &str) -> Evaluation {
        evaluate(
            &bundled(id),
            &Snapshot {
                content: screen,
                osc_title,
                osc_progress,
            },
        )
    }

    fn matched(evaluation: &Evaluation) -> Option<&str> {
        evaluation
            .matched_rule
            .as_ref()
            .map(|rule| rule.id.as_str())
    }

    /// herdr's `default_known_agent_idle_fallback`: no rule matched a known agent, so
    /// the verdict is idle with nothing to point at and no visible evidence.
    fn assert_idle_fallback(evaluation: &Evaluation, context: &str) {
        assert_eq!(evaluation.state, AgentState::Idle, "{context}");
        assert_eq!(matched(evaluation), None, "{context}");
        assert!(!evaluation.visible_idle, "{context}");
        assert!(!evaluation.visible_working, "{context}");
        assert!(!evaluation.visible_blocker, "{context}");
    }

    /// Codex's `osc_title_idle`: the terminal title is not a spinner and not "Action
    /// Required", so the agent is idle, and visibly so: written at once, not held for
    /// three reads.
    fn assert_osc_title_idle(evaluation: &Evaluation, context: &str) {
        assert_eq!(evaluation.state, AgentState::Idle, "{context}");
        assert_eq!(matched(evaluation), Some("osc_title_idle"), "{context}");
        assert!(evaluation.visible_idle, "{context}");
        assert!(!evaluation.visible_working, "{context}");
        assert!(!evaluation.visible_blocker, "{context}");
        let rule = evaluation.matched_rule.as_ref().expect("a matched rule");
        assert_eq!(rule.source, RuleSource::Manifest, "{context}");
        assert_eq!(rule.priority, 100, "{context}");
    }

    /// Whether the rule called `id` matched, whatever the verdict: every rule is reported
    /// with its evidence, and a rule that matched may still lose to a higher one.
    fn rule_matched(evaluation: &Evaluation, id: &str) -> bool {
        evaluation
            .evaluated_rules
            .iter()
            .find(|rule| rule.id == id)
            .unwrap_or_else(|| panic!("{id} is evaluated"))
            .matched
    }

    fn parse(toml_text: &str) -> Result<LoadedManifest, ManifestError> {
        parse_manifest(toml_text, ManifestSource::Bundled)
    }

    fn rules_manifest(rules: &str) -> String {
        format!(
            r#"
id = "codex"

{rules}
"#
        )
    }

    fn on(manifest: &LoadedManifest, screen: &str) -> Evaluation {
        evaluate(
            manifest,
            &Snapshot {
                content: screen,
                osc_title: "",
                osc_progress: "",
            },
        )
    }

    #[test]
    fn known_agent_no_match_defaults_to_idle_fallback() {
        let explain = explain("codex", "ordinary prompt text");

        assert_eq!(explain.state, AgentState::Idle);
        assert!(!explain.visible_idle);
        assert!(explain.matched_rule.is_none());
    }

    #[test]
    fn rule_semantics_apply_gates_priority_and_line_regex() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "low_contains"
state = "idle"
priority = 1
contains = ["match"]

[[rules]]
id = "high_nested_gates"
state = "working"
priority = 10
contains = ["match"]
all = [
  { any = [{ regex = ["w[io]n"] }, { contains = ["fallback"] }] },
]
not = [
  { contains = ["blocked"] },
]

[[rules]]
id = "line_regex"
state = "blocked"
priority = 20
line_regex = ["^exact line$"]
"#,
        ))
        .unwrap();

        let high = on(&manifest, "match win");
        assert_eq!(high.state, AgentState::Working);
        assert_eq!(matched(&high), Some("high_nested_gates"));

        let not_gate = on(&manifest, "match win blocked");
        assert_eq!(not_gate.state, AgentState::Idle);
        assert_eq!(matched(&not_gate), Some("low_contains"));

        let line = on(&manifest, "before\nexact line\nafter");
        assert_eq!(line.state, AgentState::Blocked);
        assert_eq!(matched(&line), Some("line_regex"));
    }

    // Ported from herdr src/detect/manifest/tests.rs@5608de0608 (Apache-2.0). Modifications:
    // the local override and `explain(Agent::Codex, ..)` become `parse` and `on` over the
    // same rule text.
    // A synthetic terminal protocol: live controls may precede a named divider
    // and an optional panel, but a newer prompt/output makes them historical.
    #[test]
    fn end_anchored_footer_gates_allow_only_auxiliary_tail() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "live_controls"
state = "blocked"
region = "whole_recent"
regex = ['(?i)(?:^|\n)controls:\s+(?:confirm-marker|select-marker\s+navigate-marker)\s+cancel-marker[ \t]*\n---[^\n]*(?:\npanel: \d+(?:\nrow: [^\n]+)+(?:\n… \+\d+ pending)?)?(?:\n[ \t]*)*\z']
"#,
        ))
        .unwrap();
        let state = |screen: &str| on(&manifest, screen).state;
        let footer = "controls: select-marker navigate-marker cancel-marker";
        for tail in [
            "--- name",
            "--- name\npanel: 5\nrow: active\nrow: pending\nrow: pending\nrow: pending\nrow: done\n",
            "--- name\npanel: 5\nrow: active\nrow: done\n… +3 pending",
        ] {
            let live = format!("old output\n{footer}\n{tail}");
            assert_eq!(state(&live), AgentState::Blocked, "{live}");
            for later in [
                "new output",
                "prompt:",
                "--- next\nprompt:",
                "prompt: continue…",
                "working: processing…",
                "…ordinary output",
                "finished…",
                "… +2 arbitrary words",
            ] {
                assert_eq!(
                    state(&format!("{live}\n{later}")),
                    AgentState::Idle,
                    "{live} then {later}"
                );
            }
        }
        for tail in [
            "--- name\n… +2 pending",
            "--- name\nrow: active",
            "--- name\npanel: 5",
            "--- name\npanel: 5\nrow: active\n  working: processing…",
            "--- name\npanel: 5\nrow: active\n  prompt: continue…",
        ] {
            assert_eq!(
                state(&format!("{footer}\n{tail}")),
                AgentState::Idle,
                "{tail}"
            );
        }
        for controls in [
            "controls: confirm-marker cancel-marker",
            "controls: select-marker\nnavigate-marker cancel-marker",
            "controls:\nselect-marker\nnavigate-marker\ncancel-marker",
        ] {
            assert_eq!(
                state(&format!("{controls}\n--- name")),
                AgentState::Blocked,
                "{controls}"
            );
        }
        for controls in [
            "controls: select-marker cancel-marker",
            "controls: navigate-marker cancel-marker",
            "controls: confirm-marker",
            "controls: select-marker unrelated navigate-marker cancel-marker",
            "controls: select-marker\nprompt: navigate-marker cancel-marker",
        ] {
            assert_eq!(
                state(&format!(
                    "confirm-marker navigate-marker\n{controls}\n--- name"
                )),
                AgentState::Idle,
                "{controls}"
            );
        }
    }

    // Ported from herdr src/detect/manifest/tests.rs@5608de0608 (Apache-2.0). Modifications:
    // as above.
    #[test]
    fn paired_alternatives_require_complete_gates_at_bottom_buffer() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "paired"
state = "blocked"
region = "bottom_non_empty_lines(3)"
visible_blocker = true
any = [
  { all = [
    { contains = ["alpha heading"] },
    { regex = ['alpha\s+control\s*\z'] },
  ] },
  { all = [
    { contains = ["beta heading"] },
    { regex = ['beta\s+control\s*\z'] },
  ] },
]
"#,
        ))
        .unwrap();
        for text in [
            "alpha heading\nalpha control",
            "beta heading\nbeta\ncontrol",
        ] {
            let result = on(&manifest, text);
            assert_eq!(result.state, AgentState::Blocked, "{text}");
            assert!(result.visible_blocker, "{text}");
        }
        for text in [
            "alpha heading\nbeta control",
            "beta heading\nalpha control",
            "beta control",
            "beta heading",
            "beta heading\nbeta control\nnewer content",
            "beta heading\nfiller one\nfiller two\nbeta control",
        ] {
            let result = on(&manifest, text);
            assert!(result.matched_rule.is_none(), "{text}");
            assert!(!result.visible_blocker, "{text}");
        }
    }

    #[test]
    fn contains_is_case_insensitive_and_regex_is_not() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "insensitive"
state = "blocked"
contains = ["Do You Want To Proceed?"]

[[rules]]
id = "sensitive"
state = "working"
priority = 5
regex = ["Working"]
"#,
        ))
        .unwrap();

        assert_eq!(
            matched(&on(&manifest, "do you want to proceed?")),
            Some("insensitive")
        );
        assert_eq!(matched(&on(&manifest, "working")), None);
        assert_eq!(matched(&on(&manifest, "Working")), Some("sensitive"));
    }

    #[test]
    fn ties_on_priority_keep_the_first_rule_in_file_order() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "first"
state = "working"
priority = 10
contains = ["ready"]

[[rules]]
id = "second"
state = "blocked"
priority = 10
contains = ["ready"]
"#,
        ))
        .unwrap();

        let evaluation = on(&manifest, "ready");
        assert_eq!(matched(&evaluation), Some("first"));
        assert_eq!(evaluation.state, AgentState::Working);
    }

    #[test]
    fn every_rule_is_reported_with_its_evidence() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "reported"
state = "idle"
region = "bottom_non_empty_lines(1)"
contains = ["Ready"]
any = [{ contains = ["a"] }, { contains = ["b"] }]
not = [{ contains = ["nope"] }]
"#,
        ))
        .unwrap();

        let evaluation = on(&manifest, "noise\nready a\n");
        let rule = &evaluation.evaluated_rules[0];

        assert!(rule.matched);
        assert_eq!(rule.region, "bottom_non_empty_lines(1)");
        // Evidence keeps the manifest's original casing, not the lowered form.
        assert_eq!(rule.evidence.contains, ["Ready"]);
        assert_eq!(rule.evidence.any_count, 2);
        assert_eq!(rule.evidence.not_count, 1);
        assert_eq!(rule.evidence.region_preview, "ready a\n");
        assert_eq!(rule.evidence.region_bytes, "ready a\n".len());
    }

    #[test]
    fn a_skip_state_update_rule_reports_unknown_without_visible_evidence() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "viewer"
state = "unknown"
priority = 10
skip_state_update = true
contains = ["scrollback"]
"#,
        ))
        .unwrap();

        let evaluation = on(&manifest, "scrollback");
        assert_eq!(evaluation.state, AgentState::Unknown);
        assert!(evaluation.skip_state_update);
        assert!(!evaluation.visible_idle);
        assert!(!evaluation.visible_blocker);
        assert!(!evaluation.visible_working);
    }

    // Follows herdr's src/detect/manifest/tests.rs. Differences:
    // herdr's local Codex manifest is a parsed manifest, and its `explain_with_input` and
    // `detect_agent_with_osc`, which return the matched rule and the visible flags in two
    // results, are one `evaluate` over a `Snapshot` here.
    #[test]
    fn osc_regions_use_separate_inputs_and_share_rule_priority() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "screen"
state = "idle"
priority = 10
region = "whole_recent"
visible_idle = true
contains = ["screen-marker"]

[[rules]]
id = "title"
state = "working"
priority = 20
region = "osc_title"
visible_working = true
regex = ['^title-marker$']

[[rules]]
id = "progress"
state = "blocked"
priority = 30
region = "osc_progress"
visible_blocker = true
regex = ['^progress-marker$']
"#,
        ))
        .unwrap();
        for (screen, title, progress, state, rule) in [
            ("screen-marker", "", "", AgentState::Idle, "screen"),
            (
                "screen-marker",
                "title-marker",
                "",
                AgentState::Working,
                "title",
            ),
            (
                "screen-marker",
                "title-marker",
                "progress-marker",
                AgentState::Blocked,
                "progress",
            ),
            (
                "screen-marker title-marker progress-marker",
                "",
                "",
                AgentState::Idle,
                "screen",
            ),
        ] {
            let result = evaluate(
                &manifest,
                &Snapshot {
                    content: screen,
                    osc_title: title,
                    osc_progress: progress,
                },
            );
            assert_eq!(result.state, state, "{screen} / {title} / {progress}");
            assert_eq!(
                matched(&result),
                Some(rule),
                "{screen} / {title} / {progress}"
            );
            assert_eq!(result.visible_idle, state == AgentState::Idle);
            assert_eq!(result.visible_working, state == AgentState::Working);
            assert_eq!(result.visible_blocker, state == AgentState::Blocked);
        }
        // The two fields are not interchangeable: a title in the progress field, or the
        // other way round, matches nothing.
        let swapped = evaluate(
            &manifest,
            &Snapshot {
                content: "",
                osc_title: "progress-marker",
                osc_progress: "title-marker",
            },
        );
        assert!(swapped.matched_rule.is_none());
    }

    // Follows herdr's src/detect/manifest/tests.rs. Differences:
    // as above, and herdr's `skipped_update_reason` ("matched_rule:overlay"), a string an
    // `Evaluation` has no field for, is the matched rule's id.
    #[test]
    fn skip_rule_suppresses_state_update_without_visible_state_evidence() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "activity"
state = "working"
priority = 10
visible_working = true
contains = ["activity-marker"]

[[rules]]
id = "overlay"
state = "unknown"
priority = 20
skip_state_update = true
contains = ["overlay-marker"]
"#,
        ))
        .unwrap();
        let result = on(&manifest, "activity-marker overlay-marker");

        assert_eq!(result.state, AgentState::Unknown);
        assert!(result.skip_state_update);
        assert_eq!(matched(&result), Some("overlay"));
        assert!(!result.visible_idle);
        assert!(!result.visible_working);
        assert!(!result.visible_blocker);
    }

    // Follows herdr's src/detect/manifest/tests.rs. Differences:
    // herdr's `region(input, spec)` is `region_text(spec, &snapshot)`, which returns
    // `Some` for a region the engine knows.
    #[test]
    fn screen_regions_extract_structure_without_classifying_agent_state() {
        for (screen, spec, expected) in [
            ("old\n\nnew\n", "bottom_lines(2)", "\nnew\n"),
            (
                "before\n› input\nafter\n",
                "after_last_prompt_marker",
                "after\n",
            ),
            (
                "before\n› input\nafter\n",
                "before_current_prompt_marker",
                "before\n",
            ),
            (
                "before\n› input\nafter\n",
                "whole_recent_without_current_prompt_marker",
                "",
            ),
            (
                "no marker\n",
                "whole_recent_without_current_prompt_marker",
                "no marker\n",
            ),
            (
                "• old\n■ latest\n› input\n",
                "current_prompt_block_marker",
                "■ latest",
            ),
            (
                "• old\n■ latest\n› input\n",
                "after_current_prompt_block_marker",
                "■ latest\n› input\n",
            ),
            ("› old\n• new\n", "current_prompt_block_marker", ""),
            (
                "above\n\n───\nbody\n───\nfooter\n",
                "above_prompt_box",
                "above\n\n",
            ),
            (
                "above\n\n───\nbody\n───\nfooter\n",
                "last_non_empty_above_prompt_box",
                "above",
            ),
            (
                "above\n───\nbody\n───\nfooter\n",
                "prompt_box_body",
                "body\n",
            ),
            (
                "above\n───\nbody\n───\nfooter\n",
                "after_last_horizontal_rule",
                "footer\n",
            ),
        ] {
            assert_eq!(
                region_text(
                    spec,
                    &Snapshot {
                        content: screen,
                        osc_title: "",
                        osc_progress: "",
                    },
                ),
                Some(expected),
                "region={spec}"
            );
        }
    }

    #[test]
    fn all_bundled_manifests_parse_and_validate() {
        for id in identity::bundled_ids() {
            let text = identity::bundled_manifest_text(id)
                .unwrap_or_else(|| panic!("missing bundled manifest for {id}"));
            parse_manifest(text, ManifestSource::Bundled)
                .unwrap_or_else(|err| panic!("bundled {id} manifest is invalid: {err}"));
        }
    }

    #[test]
    fn devin_manifest_detects_idle_working_and_blocked_states() {
        let idle = explain(
            "devin",
            "─────────────────────────────────────────────────────\n❭ Ask Devin to build features, fix bugs, or work on\n  your code\n─────────────────────────────────────────────────────\nSWE-1.6               Context: 16k / 200k tokens (7%)",
        );
        assert_eq!(idle.state, AgentState::Idle);
        assert!(idle.visible_idle);

        let live_footer_idle = explain(
            "devin",
            "Done.\n\n────────────────────────────────────────────────── (bypass permissions on) ─\n❭\n────────────────────────────────────────────────────────────────────────────\nClaude Opus 4.6 Thinking                                    Context: 38k / 200k tokens (18%)",
        );
        assert_eq!(live_footer_idle.state, AgentState::Idle);
        assert_eq!(matched(&live_footer_idle), Some("live_prompt_footer"));
        assert!(live_footer_idle.visible_idle);

        let welcome_footer_idle = explain(
            "devin",
            "⠀⠀⠀⠀⠀⣴⣾⣶⡄⠀⠀⠀⠀\n⠀⣴⣾⣶⡾⠛⠿⠟⠃⣴⣾⣶⡄  Devin CLI\n⠀⠛⠿⠟⠃⣴⣾⣶⡾⠛⠿⠟⠃  v2026.5.26-8\n⠀⣤⣶⣦⡄⠻⢿⠿⢷⣤⣶⣦⡄\n⠀⠻⢿⠿⢷⣤⣶⣦⡄⠻⢿⠿⠃  Hybrid\n⠀⠀⠀⠀⠀⠻⢿⠿⠃⠀⠀⠀⠀\n\n───────────────────────────\n❭ Ask Devin to build\n  features, fix bugs, or\n  work on your code\n───────────────────────────\nClaude Opus Looking for\n4.6 Thinkingplan mode? /\n            plan",
        );
        assert_eq!(welcome_footer_idle.state, AgentState::Idle);
        assert_eq!(matched(&welcome_footer_idle), Some("welcome_prompt_footer"));
        assert!(welcome_footer_idle.visible_idle);

        let working = explain(
            "devin",
            "◔ Reading shell 91b655\n  │ Timeout: 35s\n\n⠀⡆ Running tools · 27s (esc to interrupt)\n─────────────────────────────────────────────────────\n❭ Guide Devin while it works",
        );
        assert_eq!(working.state, AgentState::Working);
        assert!(working.visible_working);

        let trust_prompt = explain(
            "devin",
            "Do you trust the authors of this directory?\nFor security, devin should not be run in directories\nwith untrusted content.\n❭ 1 Yes, trust /private/tmp/devin-hook-probe\n· 2 No, exit",
        );
        assert_eq!(trust_prompt.state, AgentState::Blocked);
        assert!(trust_prompt.visible_blocker);

        let permission_prompt = explain(
            "devin",
            "⏺ Running command\n  └ $ sleep 30\n\n❭ 1 Yes  (Approve once)\n· 2 Yes, allow `sleep` commands\n· 3 Yes, always allow `sleep` commands\n· 4 No\n↑↓ select · ↵ confirm · esc cancel",
        );
        assert_eq!(permission_prompt.state, AgentState::Blocked);
        assert!(permission_prompt.visible_blocker);
    }

    #[test]
    fn muse_manifest_requires_complete_live_controls() {
        let working = explain(
            "muse",
            "⟩ hello\n\n◆ Working (0s · esc to interrupt)\n\n────────────────\n⟩\n────────────────\ngpt-5.4 · minimal · /workspace",
        );
        assert_eq!(working.state, AgentState::Working);
        assert!(working.visible_working);

        let picker = explain(
            "muse",
            "Which option should I use?\n\n› 1. Alpha\n  2. Beta\n\nEnter to select · ↑/↓ to move · Tab for an optional note · Esc to interrupt\n\n────────────────\n⟩\n────────────────\ngpt-5.4 · minimal · /workspace",
        );
        assert_eq!(picker.state, AgentState::Blocked);
        assert!(picker.visible_blocker);

        let command_approval = explain(
            "muse",
            "Would you like to run the following command?\n\n$ printf muse-safe-probe\n\n› 1. Allow this stage once (y)\n  2. Always allow in this workspace: printf muse-safe-probe ... (p)\n  3. Abort the entire command (esc)\n────────────────\ngpt-5.4 · minimal · /workspace",
        );
        assert_eq!(command_approval.state, AgentState::Blocked);
        assert!(command_approval.visible_blocker);

        let network_approval = explain(
            "muse",
            "network: example.com:443 https\nrequested by:\n$ curl -fsS https://example.com\n\n› 1. Yes, proceed (y)\n  2. Yes, don't ask again this session (p)  example.com:443 (https)\n  3. No, and tell Muse Code what to do differently (esc)\n────────────────\ngpt-5.4 · minimal · /workspace",
        );
        assert_eq!(network_approval.state, AgentState::Blocked);
        assert!(network_approval.visible_blocker);

        let menu = explain(
            "muse",
            "Theme\n\n⟩ Default (active)\n  Dynamic\n\n↑↓ move · enter save · esc go back",
        );
        assert_eq!(menu.state, AgentState::Unknown);
        assert!(menu.skip_state_update);
        assert!(!menu.visible_blocker);

        let ordinary_reply = explain(
            "muse",
            "⟩ say the phrase\n\n◆ Yes, proceed\n\n────────────────\n⟩\n────────────────\ngpt-5.4 · minimal · /workspace",
        );
        assert_eq!(ordinary_reply.state, AgentState::Idle);
        assert!(ordinary_reply.visible_idle);
    }

    #[test]
    fn manifest_validation_rejects_unknown_fields_empty_rules_invalid_regions_and_regexes() {
        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "typo"
state = "working"
contain = ["Working"]
"#
            )
            .is_err()
        );

        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "empty"
state = "working"
"#
            )
            .is_err()
        );

        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "bad_region"
state = "working"
region = "after_last_promt_marker"
contains = ["Working"]
"#
            )
            .is_err()
        );

        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "bad_regex"
state = "working"
regex = ["["]
"#
            )
            .is_err()
        );

        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "bad_nested_regex"
state = "working"
any = [{ line_regex = ["["] }]
"#
            )
            .is_err()
        );
    }

    #[test]
    fn manifest_validation_keeps_skip_rules_neutral() {
        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "bad_skip_state"
state = "idle"
skip_state_update = true
contains = ["menu"]
"#
            )
            .is_err()
        );

        assert!(
            parse(
                r#"
id = "codex"

[[rules]]
id = "bad_skip_visible"
state = "unknown"
skip_state_update = true
visible_blocker = true
contains = ["menu"]
"#
            )
            .is_err()
        );
    }

    #[test]
    fn manifest_validation_rejects_excessive_rule_count() {
        let mut manifest = String::from(
            r#"
id = "codex"
"#,
        );
        for index in 0..129 {
            manifest.push_str(&format!(
                r#"
[[rules]]
id = "rule_{index}"
state = "idle"
contains = ["ready"]
"#
            ));
        }

        assert!(parse(&manifest).is_err());
    }

    #[test]
    fn manifest_validation_rejects_excessive_gate_depth() {
        let manifest = r#"
id = "codex"

[[rules]]
id = "deep"
state = "idle"
contains = ["ready"]
all = [
  { contains = ["1"], all = [
    { contains = ["2"], all = [
      { contains = ["3"], all = [
        { contains = ["4"], all = [
          { contains = ["5"], all = [
            { contains = ["6"], all = [
              { contains = ["7"], all = [
                { contains = ["8"], all = [
                  { contains = ["9"] },
                ] },
              ] },
            ] },
          ] },
        ] },
      ] },
    ] },
  ] },
]
"#;

        assert!(parse(manifest).is_err());
    }

    #[test]
    fn manifest_validation_rejects_excessive_matchers() {
        let matchers = (0..33)
            .map(|index| format!(r#""m{index}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            r#"
id = "codex"

[[rules]]
id = "many"
state = "idle"
contains = [{matchers}]
"#
        );

        assert!(parse(&manifest).is_err());
    }

    #[test]
    fn top_non_empty_lines_requires_engine_three_when_declared() {
        let manifest = r#"
id = "grok"
version = "1"
min_engine_version = 2

[[rules]]
id = "background"
state = "working"
region = " top_non_empty_lines(1) "
contains = ["active"]
"#;

        assert!(parse(manifest).is_err());
    }

    #[test]
    fn manifests_needing_a_newer_engine_are_rejected() {
        let manifest = format!(
            r#"
id = "codex"
version = "1"
min_engine_version = {}

[[rules]]
id = "future"
state = "idle"
contains = ["ready"]
"#,
            MANIFEST_ENGINE_VERSION + 1
        );

        assert!(parse(&manifest).is_err());
    }

    // -----------------------------------------------------------------------
    // Claude's screen rules, 2026.09.11.1: the tests herdr added after v0.8.2.
    // -----------------------------------------------------------------------

    #[test]
    fn claude_idle_prompt_with_background_shell_is_idle() {
        // Captured from Claude Code 2.1.251 after its foreground turn ended while
        // a long-lived background shell remained active (issue #3414).
        let screen = concat!(
            "✻ Sautéed for 10s · 1 shell still running\n\n",
            "──────────────────────────────────────────────────────── WINDOWS ─\n",
            "❯\n",
            "────────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ auto mode on · 1 shell · ← for agents                     /rc\n",
        );
        let result = osc_explain("claude", screen, "", "");

        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("live_prompt_box"));
        assert!(result.visible_idle);
        assert!(!result.visible_working);
    }

    #[test]
    fn claude_background_shell_without_foreground_evidence_is_idle_fallback() {
        let result = osc_explain(
            "claude",
            "  ⏵⏵ auto mode on · 1 shell · ← for agents\n",
            "",
            "",
        );

        assert_eq!(result.state, AgentState::Idle);
        assert!(result.matched_rule.is_none());
        assert!(!result.visible_working);
    }

    #[test]
    fn claude_live_turn_with_background_shell_remains_working() {
        let screen = concat!(
            "────────────────────────────────────────────────────────────────\n",
            "❯\n",
            "────────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ auto mode on · 1 shell · esc to interrupt\n",
        );
        let result = osc_explain("claude", screen, "", "");

        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("live_turn_working"));
        assert!(result.visible_working);
    }

    #[test]
    fn claude_blocker_with_background_shell_remains_blocked() {
        let screen = concat!(
            "do you want to proceed?\n",
            "bash command: rm -rf /tmp/test\n",
            "❯ 1. Yes\n",
            "  2. No\n\n",
            "Esc to cancel · Tab to amend · ctrl+e to explain\n",
            "  ⏵⏵ auto mode on · 1 shell · ← for agents\n",
        );
        let result = osc_explain("claude", screen, "", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("bash_permission_prompt"));
        assert!(result.visible_blocker);
        assert!(!result.visible_working);
    }

    #[test]
    fn claude_bash_prompt_with_dont_ask_again_option_matches_bash_rule() {
        // Captured from a Bash approval prompt at its resting cursor position. The
        // "don't ask again" choice pushes No to option 3, so the only cursor-free
        // option line is one bash_permission_prompt used not to cover, which let
        // the narrower generic_permission_prompt claim the prompt instead (#2650).
        let screen = concat!(
            "────────────────────────────────────────────────────────────────\n",
            " Bash command\n\n",
            "   curl -sS -o /tmp/probe.html https://example.com\n",
            "   Download example.com to /tmp/probe.html\n\n",
            " This command requires approval\n\n",
            " Do you want to proceed?\n",
            " ❯ 1. Yes\n",
            "   2. Yes, and don't ask again for: curl *\n",
            "   3. No\n\n",
            " Esc to cancel · Tab to amend · ctrl+e to explain\n",
        );
        let result = osc_explain("claude", screen, "", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("bash_permission_prompt"));
        assert!(result.visible_blocker);
    }

    #[test]
    fn claude_permission_prompt_matches_at_every_cursor_position() {
        // The selected option carries "❯", so no option branch may assume its line
        // is cursor-free. Walk the cursor across both option layouts.
        let layouts: [&[&str]; 2] = [
            &[" ❯ 1. Yes", "   2. No"],
            &[
                " ❯ 1. Yes",
                "   2. Yes, and don't ask again for: curl *",
                "   3. No",
            ],
        ];

        for layout in layouts {
            for selected in 0..layout.len() {
                let options: Vec<String> = layout
                    .iter()
                    .enumerate()
                    .map(|(index, line)| {
                        let bare = line.trim_start().trim_start_matches('❯').trim_start();
                        if index == selected {
                            format!(" ❯ {bare}")
                        } else {
                            format!("   {bare}")
                        }
                    })
                    .collect();
                let screen = format!(
                    concat!(
                        "────────────────────────────────────────────────────────────────\n",
                        " Bash command\n\n",
                        "   curl -sS https://example.com\n\n",
                        " Do you want to proceed?\n",
                        "{}\n\n",
                        " Esc to cancel · Tab to amend · ctrl+e to explain\n",
                    ),
                    options.join("\n"),
                );
                let result = osc_explain("claude", &screen, "", "");

                assert_eq!(
                    matched(&result),
                    Some("bash_permission_prompt"),
                    "{options:?} selected={selected}"
                );
                assert_eq!(result.state, AgentState::Blocked, "selected={selected}");
                assert!(result.visible_blocker, "selected={selected}");
            }
        }
    }

    #[test]
    fn claude_mcp_elicitation_is_blocked() {
        // Regression for issue #3283: an MCP elicitation dialog has Accept/Decline
        // controls and an "Esc to cancel" footer but no Enter hint, so no blocked
        // rule matched and the static OSC title reported idle.
        // Live capture uses curly quotes around the server name; the issue report
        // transcribed straight quotes. Both must classify as blocked.
        for screen in [
            "MCP server \u{201c}my-server\u{201d} requests your input\n\nGrant temporary access to the demo gateway for 15 minutes?\n\n\u{276f} Accept    Decline\n\nEsc to cancel \u{b7} \u{2191}/\u{2193} to navigate\n",
            "MCP server \"my-server\" requests your input\n\nserver-supplied message\n\n\u{276f} Accept    Decline\n\nEsc to cancel \u{b7} \u{2191}/\u{2193} to navigate\n",
        ] {
            let result = osc_explain("claude", screen, "\u{2733} Claude Code", "");
            assert_eq!(result.state, AgentState::Blocked, "{result:#?}");
            assert!(result.visible_blocker, "{result:#?}");
            assert_eq!(
                matched(&result),
                Some("mcp_elicitation_prompt"),
                "{result:#?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // OSC rule tests — exercise the osc_title / osc_progress regions against the
    // bundled Claude and Codex manifests.
    // -----------------------------------------------------------------------

    #[test]
    fn claude_osc_title_braille_prefix_is_working() {
        // "⠂" is U+2802, in the braille block U+2800-U+28FF
        let result = osc_explain("claude", "", "⠂ project", "");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("osc_title_working"));
        assert!(result.visible_working);
    }

    #[test]
    fn claude_osc_title_half_circle_frames_are_working() {
        for frame in ['◐', '◓', '◑', '◒'] {
            let title = format!("{frame} Initial conversation with Claude");
            let result = osc_explain("claude", "", &title, "");
            assert_eq!(result.state, AgentState::Working, "frame {frame}");
            assert_eq!(matched(&result), Some("osc_title_working"), "frame {frame}");
            assert!(result.visible_working, "frame {frame}");
        }
    }

    #[test]
    fn claude_osc_title_static_prefix_is_idle() {
        // "✳" is U+2733, static prefix when Claude is not working
        let result = osc_explain("claude", "", "✳ Claude Code", "");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("osc_title_idle"));
        assert!(result.visible_idle);
    }

    #[test]
    fn claude_osc_progress_4_3_alone_does_not_force_working() {
        // Claude leaves progress stuck at 4;3 while waiting for permission, so
        // 4;3 must not be a working signal on its own. With no other evidence it
        // falls back to idle; blocked screen rules can win when present.
        let result = osc_explain("claude", "", "", "4;3;");
        assert_eq!(result.state, AgentState::Idle);
        assert!(result.matched_rule.is_none());
        assert!(!result.visible_working);
    }

    #[test]
    fn claude_blocker_screen_outranks_stale_osc_progress() {
        // Regression: progress 4;3 persists during permission prompts. The
        // blocked form on screen must win because no rule treats 4;3 as working.
        let blocker_screen =
            "──────────\n  1. Yes\n  2. No\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";
        let result = osc_explain("claude", blocker_screen, "✳ Task title", "4;3;");
        assert_eq!(result.state, AgentState::Blocked);
        assert!(result.visible_blocker);
    }

    #[test]
    fn claude_osc_progress_4_0_is_idle() {
        let result = osc_explain("claude", "", "", "4;0;");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("osc_progress_idle"));
    }

    #[test]
    fn claude_blocker_screen_outranks_osc_idle_title() {
        // When the OSC title shows ✳ (idle) but the screen has a bash permission
        // prompt, the blocked rule at priority 850 beats osc_title_idle at 250.
        let blocker_screen = "do you want to proceed?\n\
            bash command: rm -rf /tmp/test\n\
            ❯ 1. Yes\n   2. No\n\n\
            Esc to cancel · Tab to amend · ctrl+e to explain\n";
        let result = osc_explain("claude", blocker_screen, "✳ Claude Code", "");
        assert_eq!(result.state, AgentState::Blocked);
        assert!(result.visible_blocker);
    }

    #[test]
    fn claude_empty_osc_empty_screen_is_idle_fallback() {
        // No OSC data, no matching screen rule → fallback idle.
        let result = osc_explain("claude", "", "", "");
        assert_eq!(result.state, AgentState::Idle);
        assert!(result.matched_rule.is_none());
        assert!(!result.visible_idle);
    }

    #[test]
    fn codex_osc_title_braille_spinner_is_working() {
        // "⠋" is U+280B, in the braille block
        let result = osc_explain("codex", "", "⠋ llm-proxy", "");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("osc_title_working"));
        assert!(result.visible_working);
    }

    #[test]
    fn codex_osc_title_action_required_is_blocked() {
        let result = osc_explain("codex", "", "[ . ] Action Required | llm-proxy", "");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("osc_title_blocked"));
        assert!(result.visible_blocker);
    }

    #[test]
    fn codex_osc_title_plain_is_idle() {
        let result = osc_explain("codex", "", "llm-proxy", "");
        assert_osc_title_idle(&result, "plain title");
    }

    // The rule reads the title and nothing else, and a title with nothing in it says
    // nothing: a known agent that matched no rule reads idle through the fallback, with
    // no rule to point at and no visible evidence.
    #[test]
    fn codex_without_a_title_falls_back_to_idle() {
        for title in ["", " ", "\t ", "  \t"] {
            let result = osc_explain("codex", "", title, "");
            assert_idle_fallback(&result, &format!("title {title:?}"));
        }
    }

    // The title is idle unless it carries a spinner frame set off by spaces (a turn) or
    // "Action Required" (an approval). A frame glued to other characters is no spinner.
    #[test]
    fn codex_osc_title_idle_gives_way_to_a_spinner_and_to_action_required() {
        for title in [
            "project",
            "project | Ready",
            "[ . ] project",
            "⠋project",
            "project⠋",
            "⠋⠙ project",
        ] {
            assert_osc_title_idle(&osc_explain("codex", "", title, ""), title);
        }
        for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            for title in [
                format!("{frame} project"),
                format!("project | {frame}"),
                format!("{frame}"),
            ] {
                let result = osc_explain("codex", "", &title, "");
                assert_eq!(result.state, AgentState::Working, "{title}");
                assert_eq!(matched(&result), Some("osc_title_working"), "{title}");
                // Not only outranked: the idle rule itself does not take such a title.
                assert!(!rule_matched(&result, "osc_title_idle"), "{title}");
            }
        }
        let blocked = osc_explain("codex", "", "[ . ] Action Required | project", "");
        assert_eq!(blocked.state, AgentState::Blocked);
        assert_eq!(matched(&blocked), Some("osc_title_blocked"));
        assert!(!rule_matched(&blocked, "osc_title_idle"));
    }

    // The title rule is the lowest of Codex's rules: whatever the screen shows beats it. Every
    // rule of the manifest that reads the screen is drawn once, under a title that the rule
    // takes for idle, and the table is held to the manifest's own list, so a rule that herdr
    // adds fails this test until it has a screen here.
    #[test]
    fn codex_osc_title_idle_loses_to_every_screen_rule() {
        let chooser = "Update available! 0.153.0 -> 9.8.7\n\
            Run bun add -g @openai/codex to update.\n\n\
            › 1. Update now\n\
              2. Skip until next version\n\n\
            Press enter to continue   \n";
        let old_trust_dialog = "> You are in /Users/user/project\n\n\
            Do you trust the contents of this directory? Working with untrusted\n\
            contents comes with higher risk of prompt injection.\n\n\
            › 1. Yes, continue\n\
              2. No, quit\n\n\
            Press enter to continue\n";
        let transcript = "• Working (4s • esc to interrupt)\n\
            › transcript\n\
            ↑/↓ to scroll · pgup/pgdn to move · home/end to jump · q to quit · esc to edit prev\n";
        let screens = [
            ("transcript_viewer", transcript, AgentState::Unknown),
            ("trust_directory", old_trust_dialog, AgentState::Blocked),
            ("startup_update", chooser, AgentState::Blocked),
            (
                "live_strong_blocker",
                "› 1. Yes, proceed\nPress enter to confirm or esc to cancel\n",
                AgentState::Blocked,
            ),
            (
                "weak_blocker",
                "do you want to continue? [y/n]\n",
                AgentState::Blocked,
            ),
            (
                "screen_working_fallback",
                "• Working (12s • esc to interrupt)\n\n› Ask Codex to do anything\n",
                AgentState::Working,
            ),
        ];

        let mut manifest_rules: Vec<String> = osc_explain("codex", "", "", "")
            .evaluated_rules
            .into_iter()
            .map(|rule| rule.id)
            .filter(|id| !id.starts_with("osc_title_"))
            .collect();
        manifest_rules.sort();
        let mut drawn: Vec<&str> = screens.iter().map(|(id, ..)| *id).collect();
        drawn.sort();
        assert_eq!(
            manifest_rules, drawn,
            "the manifest's screen rules and the screens drawn here are not the same list"
        );

        for (rule, screen, state) in screens {
            let result = osc_explain("codex", screen, "project", "");
            assert_eq!(matched(&result), Some(rule), "{screen}");
            assert_eq!(result.state, state, "{rule}");
            assert!(!result.visible_idle, "{rule}");
            // The title rule took the title too, and lost: it is outranked, not silent.
            assert!(rule_matched(&result, "osc_title_idle"), "{rule}");
        }
    }

    #[test]
    fn codex_trust_directory_requires_live_top_region() {
        let screen = "> You are in C:\\Users\\user\\project\n\n\
            Do you trust the contents of this\n\
            directory? Working with untrusted\n\
            contents comes with higher risk of\n\
            prompt injection. Trusting the\n\
            directory allows project-local config,\n\
            hooks, and exec policies to load.\n\n\
            › 1. Yes, continue\n\
              2. No, quit\n\n\
            Press enter to continue\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("trust_directory"));
        assert!(result.visible_blocker);

        let transcript = "› > You are in C:\\Users\\user\\project\n\n\
            Do you trust the contents of this\n\
            directory? Working with untrusted contents comes with higher risk.\n";
        let result = osc_explain("codex", transcript, "project", "");

        assert_eq!(result.state, AgentState::Idle);
        assert_ne!(matched(&result), Some("trust_directory"));
        assert!(!result.visible_blocker);
    }

    // Codex 2026.10.01.1 reads the trust dialog newer Codex versions draw: "Folder
    // access" in place of the `> You are in` line, and "Trust this folder?" with the
    // sentence that says what trusting allows in place of the old question. Written for
    // taw from the rule's own strings; herdr has no Rust test for it.
    const CODEX_FOLDER_ACCESS_BODY: &str = "Folder access\n\n\
        You are in /Users/user/project\n\n\
        Trust this folder? Codex can read, edit, and run files here.\n\n";

    #[test]
    fn codex_folder_access_trust_dialog_is_blocked_by_either_footer() {
        for footer in [
            "› 1. Trust and continue\n  2. Quit\n",
            "↑/↓ select · enter continue · esc quit\n",
        ] {
            let screen = format!("{CODEX_FOLDER_ACCESS_BODY}{footer}");
            let result = osc_explain("codex", &screen, "project", "");

            assert_eq!(result.state, AgentState::Blocked, "{screen}");
            assert_eq!(matched(&result), Some("trust_directory"), "{screen}");
            assert!(result.visible_blocker, "{screen}");
            assert!(!result.visible_idle, "{screen}");
        }
    }

    #[test]
    fn codex_folder_access_trust_dialog_needs_every_part_of_it() {
        let footer = "› 1. Trust and continue\n  2. Quit\n";
        let whole = format!("{CODEX_FOLDER_ACCESS_BODY}{footer}");
        for (name, screen) in [
            ("no heading", whole.replace("Folder access", "Access")),
            (
                "no question",
                whole.replace("Trust this folder?", "Allow this folder?"),
            ),
            (
                "no sentence about what trusting allows",
                whole.replace(
                    "Codex can read, edit, and run files here",
                    "Codex works here",
                ),
            ),
            (
                "neither footer",
                whole
                    .replace("Trust and continue", "Yes")
                    .replace("enter continue", ""),
            ),
        ] {
            let result = osc_explain("codex", &screen, "project", "");
            assert_ne!(matched(&result), Some("trust_directory"), "{name}");
            assert!(!result.visible_blocker, "{name}");
            assert_osc_title_idle(&result, name);
        }
    }

    #[test]
    fn codex_folder_access_trust_dialog_requires_live_top_region() {
        // The dialog is the top of a fresh screen. The same words under twenty lines of
        // a conversation are a transcript that quotes it.
        let transcript = format!(
            "{}{CODEX_FOLDER_ACCESS_BODY}› 1. Trust and continue\n",
            "earlier output\n".repeat(20)
        );
        let result = osc_explain("codex", &transcript, "project", "");

        assert_ne!(matched(&result), Some("trust_directory"));
        assert!(!result.visible_blocker);
    }

    #[test]
    fn codex_background_terminal_screen_does_not_override_osc_idle() {
        // Background terminal tasks can be long-lived helpers such as dev servers.
        // They should not make Codex look busy once the foreground turn is idle.
        let screen = "background terminal running · /ps to view · /stop to close\n";
        let result = osc_explain("codex", screen, "llm-proxy", "");
        assert_osc_title_idle(&result, "background terminal chip");
    }

    #[test]
    fn codex_screen_working_fallback_handles_static_osc_title() {
        let screen = "• I’ll run it and wait for completion.\n\n\
            ◦ Working (1m 16s • esc to interrupt) · 1 background…\n\n\
            › Use /skills to list available skills\n\n\
            gpt-5.6-sol default · /work\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("screen_working_fallback"));
        assert!(result.visible_working);
    }

    #[test]
    fn codex_osc_working_remains_preferred_over_screen_fallback() {
        let screen = "• Working (4s • esc to interrupt)\n\n\
            › Use /skills to list available skills\n\n\
            gpt-5.6-sol default · /work\n";
        let result = osc_explain("codex", screen, "⠸ project", "");

        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("osc_title_working"));
        assert!(result.visible_working);
    }

    #[test]
    fn codex_screen_blocker_outranks_working_fallback() {
        let screen = "• Working (4s • esc to interrupt)\n\
            › 1. Yes, proceed\n\
            Press enter to confirm or esc to cancel\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("live_strong_blocker"));
        assert!(result.visible_blocker);
        assert!(!result.visible_working);
    }

    #[test]
    fn codex_weak_blocker_outranks_working_fallback() {
        let screen = "• Working (4s • esc to interrupt)\n\
            do you want to continue? [y/n]\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("weak_blocker"));
        assert!(!result.visible_working);
    }

    #[test]
    fn codex_transcript_viewer_outranks_working_fallback() {
        let screen = "• Working (4s • esc to interrupt)\n\
            › transcript\n\
            ↑/↓ to scroll · pgup/pgdn to move · home/end to jump · q to quit · esc to edit prev\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Unknown);
        assert_eq!(matched(&result), Some("transcript_viewer"));
        assert!(result.skip_state_update);
        assert!(!result.visible_working);
    }

    #[test]
    fn codex_screen_working_fallback_ignores_stale_and_prompt_text() {
        let screens = [
            "◦ Working (1m 16s • esc to interrupt)\n\
             ■ Conversation interrupted\n\
             › Use /skills to list available skills\n\
             gpt-5.6-sol default · /work\n",
            "› Explain the text ◦ Working (1m 16s • esc to interrupt)\n\
             gpt-5.6-sol default · /work\n",
            "  ◦ Working (1m 16s • esc to interrupt)\n\
             › Use /skills to list available skills\n\
             gpt-5.6-sol default · /work\n",
            "Working (1m 16s • esc to interrupt)\n\
             • Finished the task\n\
             › Ask Codex to do anything\n",
            "Working (1m 16s • esc to interrupt)\n\
             ■ Conversation interrupted\n\
             › Ask Codex to do anything\n",
            "Working (1m 16s • esc to interrupt)\n\
             ─ Worked for 1m 16s ─\n\
             › Ask Codex to do anything\n",
            "Working (1m 16s • esc to interrupt)\n•\nMessages to be submitted after next tool call\n› Ask Codex to do anything\n",
            "› Explain this status:\n  Working (1m 16s • esc to interrupt)\n",
            "• Example (press esc to interrupt)\n\
             › Ask Codex to do anything\n",
        ];

        for screen in screens {
            let result = osc_explain("codex", screen, "project", "");
            assert_osc_title_idle(&result, screen);
        }
    }

    #[test]
    fn codex_screen_working_fallback_ignores_interrupted_short_terminal() {
        let screen = "◦ Working (1m 16s • esc to interrupt)\n\
            ■ Conversation interrupted\n\
            ›\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_osc_title_idle(&result, "interrupted short terminal");
    }

    #[test]
    fn codex_osc_working_beats_weak_blocker_screen() {
        // A stale [y/n] on screen triggers weak_blocker at priority 600, but an
        // active braille spinner in the OSC title is priority 1050 — OSC wins.
        let screen = "do you want to continue? [y/n]\n";
        let result = osc_explain("codex", screen, "⠋ llm-proxy", "");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("osc_title_working"));
    }

    // -----------------------------------------------------------------------
    // Codex: the tests herdr added after v0.8.2, run against herdr master's manifest
    // (2026.10.01.1).
    // -----------------------------------------------------------------------

    #[test]
    fn codex_startup_update_requires_complete_live_chooser() {
        let chooser = "Update available! 0.153.0 -> 9.8.7\n\
            Run bun add -g @openai/codex to update.\n\n\
            › 1. Update now\n\
              2. Skip until next version\n\n\
            Press enter to continue   \n";
        let wrapped = "✨ Update available! 0.153.0\n\n\
            Release notes: https://example\n\n\
            › 1. Update now (runs `npm\n\
                 install -g\n\
                 @openai/codex`)\n\
              2. Skip\n\
              3. Skip until next\n\
                 version\n\n\
            Press enter to continue\n";

        for screen in [chooser, wrapped] {
            let result = osc_explain("codex", screen, "project", "");
            assert_eq!(result.state, AgentState::Blocked);
            assert_eq!(matched(&result), Some("startup_update"));
            assert!(result.visible_blocker);
        }

        for screen in [
            chooser.replace("Update now", "Install"),
            format!("{wrapped}\n› Ask Codex to do anything\n"),
        ] {
            let result = osc_explain("codex", &screen, "project", "");
            assert_eq!(result.state, AgentState::Idle);
            assert_ne!(matched(&result), Some("startup_update"));
            assert!(!result.visible_blocker);
        }
    }

    #[test]
    fn codex_screen_working_fallback_handles_activity_labels_and_queued_inputs() {
        for prefix in ["", "• ", "◦ "] {
            for label in ["Working", "Fixing bug in queue region"] {
                for queue in [
                    "",
                    "\n• Queued follow-up inputs\n  ↳ Follow up after this turn\n    alt + ↑ edit last queued message\n",
                    "\n• Messages to be submitted after next tool call\n  (press esc to interrupt and send immediately)\n  ↳ Keep waiting until the sleep finishes.\n",
                    "\n• Messages to be submitted after next tool call (press esc to interrupt and send immediately)\n  ↳ Keep waiting until the sleep finishes.\n",
                    "\n• Messages to be submitted after next tool call (press esc to interrupt\n  and send immediately)\n  ↳ Keep waiting until the sleep finishes.\n",
                    "\n• Messages to be submitted after next\n  tool call (press esc to interrupt and\n  send immediately)\n  ↳ Keep waiting until the sleep finishes.\n",
                    "\n• Messages to be submitted at end of turn\n  ↳ Follow up after this turn\n",
                    "\n• Messages to be submitted after next tool call\n  (press esc to interrupt and send immediately)\n  ↳ Keep waiting until the sleep finishes.\n\n• Queued follow-up inputs\n  ↳ After this turn reply ok.\n    alt + ↑ edit last queued message\n",
                ] {
                    let screen = format!(
                        "{prefix}{label} (1m 16s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close\n\
                         {queue}\n› Ask Codex to do anything\n\n  model · /work\n"
                    );
                    let result = osc_explain("codex", &screen, "project", "");

                    assert_eq!(result.state, AgentState::Working, "{screen}");
                    assert_eq!(
                        matched(&result),
                        Some("screen_working_fallback"),
                        "{screen}"
                    );
                    assert!(result.visible_working);
                }
            }
        }
    }

    #[test]
    fn codex_screen_working_fallback_uses_latest_activity_after_interruption() {
        let screen = "■ Conversation interrupted\n\n\
            › Try again\n\n\
            Working (4s • esc to interrupt)\n\n\
            › Ask Codex to do anything\n\n  model · /work\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Working);
        assert!(result.visible_working);
    }

    #[test]
    fn codex_weak_blocker_without_current_prompt_is_blocked() {
        let result = osc_explain("codex", "do you want to continue? [y/n]\n", "project", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("weak_blocker"));
    }

    #[test]
    fn codex_current_prompt_keeps_weak_text_from_overriding_working_fallback() {
        let screen = "• Working (4s • esc to interrupt)\n\
            do you want to continue? [y/n]\n\
            › Use /skills to list available skills\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("screen_working_fallback"));
        assert!(result.visible_working);
    }

    #[test]
    fn codex_weak_blocker_ignores_finished_response_above_current_prompt() {
        let screen = "• The `wt rm` transcript now shows [y/N] / esc, matching the real prompt.\n\n\
            ─ Worked for 4m 59s ─\n\n\
            › Ask Codex to do anything\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_osc_title_idle(&result, screen);
    }

    #[test]
    fn codex_weak_blocker_ignores_wrapped_current_prompt_text() {
        let screen = "› Explain why this prompt wraps before quoting the confirmation text\n\
              [y/N] / esc and whether the docs should include it\n\n\
              gpt-5.6-sol default · /work\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_osc_title_idle(&result, screen);
    }

    #[test]
    fn codex_sparkle_prompt_preserves_live_states() {
        for marker in ["› ", "›⠁", "›⠂", "›⠄", "›⠈", "›⠐", "›⠠", "›⡀", "›⢀"]
        {
            let screen = format!("Do you want to proceed? [y/n]\n{marker}unsent draft\n");
            let result = osc_explain("codex", &screen, "project | Ready", "");
            assert_eq!(result.state, AgentState::Idle, "{marker}");

            let working = format!(
                "Do you want to proceed? [y/n]\n• Working (4s • esc to interrupt)\n{marker}draft\n"
            );
            let result = osc_explain("codex", &working, "project", "");
            assert_eq!(result.state, AgentState::Working, "{marker}");

            let approval = format!("{screen}Press enter to confirm or esc to cancel\n");
            let result = osc_explain("codex", &approval, "project", "");
            assert_eq!(result.state, AgentState::Blocked, "{marker}");
            assert!(result.visible_blocker);

            for response_marker in ['•', '■', '✗', '✓'] {
                let response = format!("{screen}{response_marker} Do you want to proceed? [y/n]\n");
                let result = osc_explain("codex", &response, "project", "");
                assert_eq!(
                    result.state,
                    AgentState::Blocked,
                    "{marker} {response_marker}"
                );
            }
        }
    }

    #[test]
    fn codex_weak_blocker_does_not_ignore_arbitrary_prompt_suffixes() {
        for line in ["›text", "›⠋draft", "›⠀draft", " ›⠁draft", "quoted ›⠁draft"]
        {
            let screen = format!("Do you want to proceed? [y/n]\n{line}\n");
            let result = osc_explain("codex", &screen, "project", "");
            assert_eq!(result.state, AgentState::Blocked, "{line}");
        }
    }

    // -----------------------------------------------------------------------
    // Codex 2026.09.23.1 (herdr v0.9.2) and 2026.10.01.1 (herdr master): the rules that
    // changed after v0.9.1. Written for taw from each rule's own strings; herdr has no Rust test for
    // them.
    // -----------------------------------------------------------------------

    #[test]
    fn codex_mention_completion_popup_is_blocked_by_its_three_scopes() {
        // Below the last prompt line, all three scope names together make the popup a
        // live blocker.
        let screen = "› @src\n\nAll Results\nFilesystem Only\nPlugins\n";
        let result = osc_explain("codex", screen, "project", "");

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("live_strong_blocker"));
        assert!(result.visible_blocker);

        // Two of the three names are ordinary text.
        let partial = "› @src\n\nAll Results\nFilesystem Only\n";
        assert_osc_title_idle(&osc_explain("codex", partial, "project", ""), "two scopes");
    }

    #[test]
    fn codex_working_status_keeps_its_timer_when_the_interrupt_key_is_remapped() {
        let live = "• Working (12s • ctrl+c to interrupt)\n\n\
            › Ask Codex to do anything\n\n  model · /work\n";
        let result = osc_explain("codex", live, "project", "");

        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("screen_working_fallback"));
        assert!(result.visible_working);

        // Unbound or hidden, the timer alone still reads as live.
        let bare = "• Working (12s)\n\n› Ask Codex to do anything\n";
        let result = osc_explain("codex", bare, "project", "");
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("screen_working_fallback"));
    }

    #[test]
    fn codex_failed_reconnect_keeps_its_timer_but_is_not_working() {
        let failed = "• Reconnect failed — check the endpoint, then relaunch (3s)\n\n\
            › Ask Codex to do anything\n";
        assert_osc_title_idle(&osc_explain("codex", failed, "project", ""), "failed");

        // Without the words that say it failed, the same shape is a live status.
        let live = "• Working (3s)\n\n› Ask Codex to do anything\n";
        let result = osc_explain("codex", live, "project", "");
        assert_eq!(result.state, AgentState::Working);
    }

    // -----------------------------------------------------------------------
    // Codex, taw's own idle rule (supplements/codex.toml). herdr's manifest reads a Codex
    // at rest idle only through the terminal title (`osc_title_idle`, which says idle for
    // any title that is not a spinner or "Action Required"). Where the title says nothing,
    // a Codex that only its launch hint names (npm's runs as `node`) matches nothing at
    // rest and reads `unknown`: taw's rule reads its composer instead. The title says
    // nothing when it is empty, as it is for the first reads after another agent hands
    // the pane over (the watcher drops the title that agent left), so those tests read
    // under an empty title, and a few more read under one to show what herdr's rule says
    // of the same screens. Written for taw, from the screens the tests above draw; each
    // reads the manifest the way the watcher does, supplement included.
    // -----------------------------------------------------------------------

    /// Codex's bundled manifest with taw's supplement appended, as the loader serves it,
    /// read under `title`.
    fn codex_read_titled(screen: &str, title: &str) -> Evaluation {
        let store = crate::detect::ManifestStore::new(
            std::path::PathBuf::from("/nonexistent/taw-test-override"),
            std::path::PathBuf::from("/nonexistent/taw-test-remote"),
        );
        let manifest = store.load("codex").expect("codex loads");
        evaluate(
            &manifest,
            &Snapshot {
                content: screen,
                osc_title: title,
                osc_progress: "",
            },
        )
    }

    /// The same read under no title at all, where herdr's title rule has nothing to say
    /// and only the screen can.
    fn codex_read(screen: &str) -> Evaluation {
        codex_read_titled(screen, "")
    }

    /// Whether taw's composer rule matched, panicking when the supplement is not loaded:
    /// a test that says "not idle" must not pass for want of the rule.
    fn composer_rule_matched(evaluation: &Evaluation) -> bool {
        evaluation
            .evaluated_rules
            .iter()
            .find(|rule| rule.id == "taw_composer_idle")
            .expect("taw_composer_idle is evaluated")
            .matched
    }

    /// The verdict of taw's composer rule: idle, and a visible idle, from a taw rule of
    /// low priority.
    fn assert_composer_idle(evaluation: &Evaluation, context: &str) {
        assert_eq!(evaluation.state, AgentState::Idle, "{context}");
        assert_eq!(matched(evaluation), Some("taw_composer_idle"), "{context}");
        assert!(evaluation.visible_idle, "{context}");
        assert!(!evaluation.visible_working, "{context}");
        assert!(!evaluation.visible_blocker, "{context}");
        let rule = evaluation.matched_rule.as_ref().expect("a matched rule");
        assert_eq!(rule.source, RuleSource::Taw, "{context}");
        assert_eq!(rule.priority, 100, "{context}");
    }

    /// A Codex just started: its banner, the commands it suggests, an empty composer.
    const CODEX_JUST_STARTED: &str = "\
╭────────────────────────────────────────────────╮\n\
│ >_ OpenAI Codex (v0.128.0)                     │\n\
│                                                │\n\
│ model:     gpt-5.6-sol high   /model to change │\n\
│ directory: ~/code/project                      │\n\
╰────────────────────────────────────────────────╯\n\n\
  To get started, describe a task or try one of these commands:\n\n\
  /init - create an AGENTS.md file with instructions for Codex\n\
  /status - show current session configuration\n\n\
› Explain this codebase\n\n\
  ? for shortcuts                                100% context left\n";

    /// A Codex after a finished turn: the reply, the turn's rule, an empty composer.
    const CODEX_AFTER_A_TURN: &str = "\
• The `wt rm` transcript now shows [y/N] / esc, matching the real prompt.\n\n\
─ Worked for 4m 59s ─\n\n\
› Ask Codex to do anything\n\n\
  gpt-5.6-sol default · /work\n";

    /// Screens of a Codex at its composer with nothing running, each with a name.
    const CODEX_COMPOSER_AT_REST: [(&str, &str); 8] = [
        ("just started", CODEX_JUST_STARTED),
        ("after a finished turn", CODEX_AFTER_A_TURN),
        (
            "the skills placeholder",
            "› Use /skills to list available skills\n\ngpt-5.6-sol default · /work\n",
        ),
        ("no placeholder", "›\n"),
        (
            "after an interruption",
            "■ Conversation interrupted\n\n› Ask Codex to do anything\n",
        ),
        // A failed reconnect keeps its timer and is no turn, for the manifest as well.
        (
            "after a failed reconnect",
            "• Reconnect failed — check the endpoint, then relaunch (3s)\n\n› Ask Codex to do anything\n",
        ),
        // Nothing is running while a draft waits for Enter, over one line or two.
        (
            "an unsent draft",
            "› fix the failing test\n\n  gpt-5.6-sol default · /work\n",
        ),
        (
            "a draft over two lines",
            "› first line\n  second line\n\n  model · /work\n",
        ),
    ];

    /// Status text that the manifest does not take for a turn (its tests say so), beside
    /// a composer at rest: a response after the status line, a rule under it, an indent,
    /// or the text inside the composer itself.
    const CODEX_COMPOSER_BESIDE_STALE_STATUS: [(&str, &str); 7] = [
        (
            "an interruption after it",
            "◦ Working (1m 16s • esc to interrupt)\n■ Conversation interrupted\n› Use /skills to list available skills\ngpt-5.6-sol default · /work\n",
        ),
        (
            "a finished reply after it",
            "Working (1m 16s • esc to interrupt)\n• Finished the task\n› Ask Codex to do anything\n",
        ),
        (
            "the turn's rule after it",
            "Working (1m 16s • esc to interrupt)\n─ Worked for 1m 16s ─\n› Ask Codex to do anything\n",
        ),
        (
            "indented",
            "  ◦ Working (1m 16s • esc to interrupt)\n› Use /skills to list available skills\ngpt-5.6-sol default · /work\n",
        ),
        (
            "typed into the composer",
            "› Explain the text ◦ Working (1m 16s • esc to interrupt)\ngpt-5.6-sol default · /work\n",
        ),
        (
            "typed over a second line",
            "› Explain this status:\n  Working (1m 16s • esc to interrupt)\n",
        ),
        (
            "an example of the hint",
            "• Example (press esc to interrupt)\n› Ask Codex to do anything\n",
        ),
    ];

    #[test]
    fn codex_composer_at_rest_reads_idle_by_taw_composer_idle() {
        for (name, screen) in CODEX_COMPOSER_AT_REST {
            assert_composer_idle(&codex_read(screen), name);
        }
    }

    #[test]
    fn codex_composer_reads_idle_beside_status_text_that_is_stale_or_quoted() {
        // The manifest does not take these for a turn (its tests say so), and neither does
        // the composer rule's guard.
        for (name, screen) in CODEX_COMPOSER_BESIDE_STALE_STATUS {
            assert_composer_idle(&codex_read(screen), name);
        }
    }

    #[test]
    fn codex_composer_is_not_idle_while_a_turn_is_on_screen() {
        // The manifest's working rule outranks it; the composer rule does not match at
        // all, so it would not have been idle had that rule missed the frame.
        for (name, screen) in [
            (
                "the status line",
                "• Working (12s • esc to interrupt)\n\n› Ask Codex to do anything\n\n  model · /work\n",
            ),
            (
                "a background terminal beside it",
                "◦ Working (1m 16s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close\n\n› Ask Codex to do anything\n\n  model · /work\n",
            ),
            (
                "the interrupt key unbound",
                "• Working (12s)\n\n› Ask Codex to do anything\n",
            ),
            (
                "a queued message",
                "• Working (1m 16s • esc to interrupt)\n\n• Queued follow-up inputs\n  ↳ Follow up after this turn\n    alt + ↑ edit last queued message\n\n› Ask Codex to do anything\n",
            ),
        ] {
            let result = codex_read(screen);
            assert_eq!(result.state, AgentState::Working, "{name}");
            assert_eq!(matched(&result), Some("screen_working_fallback"), "{name}");
            assert!(result.visible_working, "{name}");
            assert!(!result.visible_idle, "{name}");
            assert!(!composer_rule_matched(&result), "{name}");
        }
    }

    #[test]
    fn codex_composer_is_not_a_visible_idle_beside_a_turn_the_manifest_cannot_read() {
        // Neither reads as working by the manifest (a hint that does not say "to
        // interrupt", a queue heading with its bullet on a line of its own), so the
        // verdict is the fallback's plain idle, which the watcher holds for three reads.
        // A visible idle from the composer rule would be written at once, as a turn's end.
        for screen in [
            "• Working (12s • ctrl+c to stop)\n\n› Ask Codex to do anything\n",
            "Working (1m 16s • esc to interrupt)\n•\nMessages to be submitted after next tool call\n› Ask Codex to do anything\n",
        ] {
            let result = codex_read(screen);
            assert_idle_fallback(&result, screen);
            assert!(!composer_rule_matched(&result), "{screen}");
        }
    }

    #[test]
    fn codex_composer_is_not_idle_at_a_prompt() {
        // An approval: the `›` is the cursor on the chosen answer, and the blocker rule
        // outranks the composer rule anyway.
        let approval = "Would you like to run the following command?\n\n\
            $ rm -rf build\n\n\
            › 1. Yes, proceed (y)\n\
              2. Yes, and don't ask again for this command (a)\n\
              3. No, and tell Codex what to do differently (esc)\n\n\
            Press enter to confirm or esc to cancel\n";
        let result = codex_read(approval);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("live_strong_blocker"));
        assert!(!composer_rule_matched(&result));

        // The mention popup leaves the composer on screen; the blocker rule is what says
        // this is not a Codex at rest.
        let popup = "› @src\n\nAll Results\nFilesystem Only\nPlugins\n";
        let result = codex_read(popup);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("live_strong_blocker"));

        // No composer to be idle at: the weak blocker.
        let question = "do you want to continue? [y/n]\n";
        let result = codex_read(question);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("weak_blocker"));
        assert!(!composer_rule_matched(&result));

        // A list of choices that no blocker rule knows, a model picker: its cursor is
        // not a composer.
        let picker = "Select Model and Effort\n\n\
            › 1. gpt-5.6-sol (current)\n\
              2. gpt-5.5\n\n\
            Press enter to select or esc to go back\n";
        let result = codex_read(picker);
        assert_idle_fallback(&result, "a picker");
        assert!(!composer_rule_matched(&result));
    }

    #[test]
    fn codex_composer_rule_needs_the_current_prompt() {
        for (name, screen) in [
            (
                "a reply after the prompt",
                "› Ask Codex to do anything\n• I will start with the tests.\n",
            ),
            ("the shell alone", "$ \n"),
            ("nothing drawn", "\n"),
            ("a sparkle prompt", "›⠁ unsent draft\n"),
            ("an indented prompt", "  › Ask Codex to do anything\n"),
        ] {
            let result = codex_read(screen);
            assert_idle_fallback(&result, name);
            assert!(!composer_rule_matched(&result), "{name}");
        }
    }

    #[test]
    fn codex_composer_rule_yields_to_an_upstream_idle_rule_at_equal_priority() {
        // herdr's 2026.09.05.1 kept Codex's idle in the title, at the same priority. A
        // supplement is appended after the manifest, so the manifest's rule wins a tie;
        // taw's reads the composer only where the manifest's has nothing to say.
        let mut manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "osc_title_idle"
state = "idle"
priority = 100
region = "osc_title"
visible_idle = true
regex = ['\S']
"#,
        ))
        .unwrap();
        manifest
            .append_supplement(crate::detect::supplements::text("codex").unwrap())
            .unwrap();

        let read = |title: &str| {
            evaluate(
                &manifest,
                &Snapshot {
                    content: CODEX_AFTER_A_TURN,
                    osc_title: title,
                    osc_progress: "",
                },
            )
        };
        let titled = read("project");
        assert_eq!(matched(&titled), Some("osc_title_idle"));
        assert_eq!(titled.matched_rule.unwrap().source, RuleSource::Manifest);
        assert_composer_idle(&read(""), "no title");
    }

    // What taw's composer rule adds is the cases herdr's title rule cannot read, so a
    // test of each case has two halves: with a title, herdr's rule says idle, and with
    // none, taw's does.

    #[test]
    fn codex_composer_rule_yields_to_the_bundled_manifests_title_rule() {
        // The same tie as above, with herdr's real manifest (2026.10.01.1) in place of a
        // stand-in: both rules match a composer at rest and say idle at priority 100, and
        // herdr's is the verdict because a supplement is appended after the manifest.
        for (name, screen) in CODEX_COMPOSER_AT_REST
            .into_iter()
            .chain(CODEX_COMPOSER_BESIDE_STALE_STATUS)
        {
            let titled = codex_read_titled(screen, "project");
            assert_osc_title_idle(&titled, name);
            assert!(composer_rule_matched(&titled), "{name}");
            assert_composer_idle(&codex_read_titled(screen, ""), name);
            // A title that is only white space says nothing either.
            assert_composer_idle(&codex_read_titled(screen, " \t "), name);
        }
    }

    #[test]
    fn codex_title_rule_reads_idle_where_the_composer_rule_has_no_composer_to_read() {
        // herdr's rule is wider: any title that is not a spinner or "Action Required" is
        // idle, with or without a composer on screen, and visibly so. taw's rule needs the
        // current prompt, so under no title these screens are the fallback's plain idle
        // (a pane named by a hint alone would read `unknown`).
        for (name, screen) in [
            (
                "a reply after the prompt",
                "› Ask Codex to do anything\n• I will start with the tests.\n",
            ),
            ("the shell alone", "$ \n"),
            ("nothing drawn", "\n"),
            ("a sparkle prompt", "›⠁ unsent draft\n"),
            ("an indented prompt", "  › Ask Codex to do anything\n"),
            (
                "a picker",
                "Select Model and Effort\n\n› 1. gpt-5.6-sol (current)\n  2. gpt-5.5\n\nPress enter to select or esc to go back\n",
            ),
            // A turn whose status line the manifest cannot read: with a title, herdr's rule
            // says idle at once, beside the turn.
            (
                "a turn the manifest cannot read",
                "• Working (12s • ctrl+c to stop)\n\n› Ask Codex to do anything\n",
            ),
        ] {
            let titled = codex_read_titled(screen, "project");
            assert_osc_title_idle(&titled, name);
            assert!(!composer_rule_matched(&titled), "{name}");
            assert_idle_fallback(&codex_read_titled(screen, ""), name);
        }
    }

    #[test]
    fn codex_turns_and_blockers_read_the_same_with_or_without_a_title() {
        // Neither idle rule outranks what the screen says is happening.
        let approval = "Would you like to run the following command?\n\n$ rm -rf build\n\n\
            › 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\n\n\
            Press enter to confirm or esc to cancel\n";
        for title in ["", "project"] {
            let working = codex_read_titled(
                "• Working (12s • esc to interrupt)\n\n› Ask Codex to do anything\n",
                title,
            );
            assert_eq!(working.state, AgentState::Working, "{title:?}");
            assert_eq!(matched(&working), Some("screen_working_fallback"));
            assert!(!working.visible_idle, "{title:?}");

            let blocked = codex_read_titled(approval, title);
            assert_eq!(blocked.state, AgentState::Blocked, "{title:?}");
            assert_eq!(matched(&blocked), Some("live_strong_blocker"));
            assert!(blocked.visible_blocker, "{title:?}");

            let weak = codex_read_titled("do you want to continue? [y/n]\n", title);
            assert_eq!(weak.state, AgentState::Blocked, "{title:?}");
            assert_eq!(matched(&weak), Some("weak_blocker"));
        }
        // And the title's own turn and approval win over a composer at rest.
        let spinner = codex_read_titled(CODEX_AFTER_A_TURN, "⠋ project");
        assert_eq!(spinner.state, AgentState::Working);
        assert_eq!(matched(&spinner), Some("osc_title_working"));
        let asking = codex_read_titled(CODEX_AFTER_A_TURN, "[ . ] Action Required | project");
        assert_eq!(asking.state, AgentState::Blocked);
        assert_eq!(matched(&asking), Some("osc_title_blocked"));
    }

    // -----------------------------------------------------------------------
    // Claude's sub-agents, taw's own working rule (supplements/claude.toml). Claude Code
    // lists its sub-agents under the prompt box, one row each, and the main agent that
    // ends its turn while they run is at its prompt: herdr's `live_prompt_box` reads that
    // idle, and the watcher writes `done` with the work unfinished. Written for taw, from
    // the footer a Claude draws while it has sub-agents; the names and the words in the
    // rows are made up, the layout is Claude's. Each test reads the manifest the way the
    // watcher does, supplement included.
    // -----------------------------------------------------------------------

    /// Claude's bundled manifest with taw's supplement appended, as the loader serves it.
    fn claude_read(screen: &str) -> Evaluation {
        let store = crate::detect::ManifestStore::new(
            std::path::PathBuf::from("/nonexistent/taw-test-override"),
            std::path::PathBuf::from("/nonexistent/taw-test-remote"),
        );
        let manifest = store.load("claude").expect("claude loads");
        evaluate(
            &manifest,
            &Snapshot {
                content: screen,
                osc_title: "\u{2733} fake task",
                osc_progress: "",
            },
        )
    }

    /// Whether the taw rule `id` matched, panicking when the supplement is not loaded: a
    /// test that says "not working" must not pass for want of the rule.
    fn taw_rule_matched(evaluation: &Evaluation, id: &str) -> bool {
        evaluation
            .evaluated_rules
            .iter()
            .find(|rule| rule.id == id)
            .unwrap_or_else(|| panic!("{id} is evaluated"))
            .matched
    }

    /// Whether taw's sub-agent rule matched.
    fn subagent_rule_matched(evaluation: &Evaluation) -> bool {
        taw_rule_matched(evaluation, "taw_subagent_running")
    }

    /// The verdict of taw's sub-agent rule: working, and a visible working, from a taw
    /// rule that sits under the turn rules and over the prompt box.
    fn assert_subagents_running(evaluation: &Evaluation, context: &str) {
        assert_eq!(evaluation.state, AgentState::Working, "{context}");
        assert_eq!(
            matched(evaluation),
            Some("taw_subagent_running"),
            "{context}"
        );
        assert!(evaluation.visible_working, "{context}");
        assert!(!evaluation.visible_idle, "{context}");
        assert!(!evaluation.visible_blocker, "{context}");
        let rule = evaluation.matched_rule.as_ref().expect("a matched rule");
        assert_eq!(rule.source, RuleSource::Taw, "{context}");
        assert_eq!(rule.priority, 960, "{context}");
        assert_eq!(rule.region, "after_last_horizontal_rule", "{context}");
    }

    /// The footer of a Claude in vim mode with a background shell and an agent list to open.
    const AGENTS_FOOTER: &str = "-- INSERT -- ⏵⏵ auto mode on · 1 shell · ← for agents";

    /// The dialogs that Claude draws with no "Esc to cancel" footer, which the permission
    /// rules read by, and that only the legacy blocker (300) reads: a plan approval, an
    /// edit approval, the review page of a questionnaire and the plan interview's skip.
    /// Each is the dialog as it stands under its own rule, with no prompt box.
    const CLAUDE_DIALOGS_WITHOUT_AN_ESC_FOOTER: [(&str, &str); 4] = [
        (
            "a plan approval",
            " Ready to code?\n\n Here is the plan:\n\n 1. Change the parser\n 2. Run the suite\n\n Would you like to proceed?\n\n ❯ 1. Yes, and auto-accept edits\n   2. Yes, and manually approve edits\n   3. No, keep planning\n",
        ),
        (
            "an edit approval",
            " Edit file\n\n src/parser.rs\n\n Do you want to make this edit to src/parser.rs?\n ❯ 1. Yes\n   2. Yes, allow all edits during this session\n   3. No, and tell Claude what to do differently\n",
        ),
        (
            "the review page",
            " Review your answers\n\n Which parser should the builder use?\n   The new one\n\n Ready to submit your answers?\n\n ❯ 1. Submit answers\n   2. Cancel\n",
        ),
        (
            "the interview's skip",
            " Before the plan, a few questions\n\n ❯ 1. Ask me first\n   2. Skip interview and plan immediately\n",
        ),
    ];

    /// A sub-agent's row as Claude draws it: `lead` is what stands before its name (the
    /// pointer column and the bullet), then the name, what it was asked cut to a fixed
    /// width, and its status against the right edge of a 100-column screen.
    fn agent_row_as(lead: &str, name: &str, status: &str) -> String {
        let head = format!("{lead} {name:<9}  {:<40}", "Check the parser change...");
        let gap = 98usize.saturating_sub(head.chars().count() + status.chars().count());
        format!("{head}{}{status}", " ".repeat(gap.max(2)))
    }

    /// A sub-agent's row in the ordinary case: not selected, not open, not nested.
    fn agent_row(name: &str, status: &str) -> String {
        agent_row_as("  ◯", name, status)
    }

    /// The prompt box at rest and its footer, then `under` as it stands: what Claude draws
    /// below the footer.
    fn claude_box_then(footer: &str, under: &str) -> String {
        let rule = "─".repeat(100);
        format!("⏺ Both agents are on it.\n\n{rule}\n❯\u{a0}\n{rule}\n  {footer}\n{under}")
    }

    /// The agent list under the footer, `main`'s row as given and then the rows, and
    /// nothing at all when there are no rows.
    fn claude_agent_list_headed(footer: &str, main: &str, rows: &[String]) -> String {
        let mut under = String::new();
        if !rows.is_empty() {
            under.push_str(&format!("\n{main}\n"));
            for row in rows {
                under.push_str(row);
                under.push('\n');
            }
        }
        claude_box_then(footer, &under)
    }

    /// The prompt box at rest, its footer, and under them the agent list when there are
    /// agents: `main` first, then the rows.
    fn claude_agent_list(footer: &str, rows: &[String]) -> String {
        claude_agent_list_headed(footer, "  ⏺ main", rows)
    }

    #[test]
    fn claude_at_its_prompt_with_a_sub_agent_still_counting_reads_working() {
        let screen = claude_agent_list(
            AGENTS_FOOTER,
            &[
                agent_row("reviewer", "idle"),
                agent_row("builder", "4m 46s · ↓ 170.2k tokens"),
            ],
        );
        assert_subagents_running(&claude_read(&screen), "one agent running, one idle");

        // herdr's rules alone read the same screen as the prompt box at rest: idle, and
        // a visible one, which the watcher writes at once as the turn's end. That is the
        // false `done` this rule is for.
        let upstream = explain("claude", &screen);
        assert_eq!(upstream.state, AgentState::Idle);
        assert_eq!(matched(&upstream), Some("live_prompt_box"));
        assert!(upstream.visible_idle);
    }

    #[test]
    fn claude_sub_agent_row_reads_working_in_every_shape_a_running_agent_draws_it() {
        // The time as Claude 2.1.284 prints it (`12s`, `4m 46s`, `1m 0s`, `1h 3m 0s`: the
        // trailing zeros stay), the tokens as it counts them (`842`, `6.0k`, `170.2k`,
        // `1.2m`, in lower case), either arrow, and the messages queued for the agent.
        for status in [
            "12s · ↓ 842 tokens",
            "4m 46s · ↓ 170.2k tokens",
            "1m 0s · ↓ 6.0k tokens",
            "1h 3m 0s · ↑ 1.2m tokens",
            "1h 3m 12s · ↓ 96.4k tokens",
            "4m 46s · ↓ 170.2k tokens · 2 queued",
        ] {
            let screen = claude_agent_list(AGENTS_FOOTER, &[agent_row("builder", status)]);
            assert_subagents_running(&claude_read(&screen), status);
        }

        // The rule is looser than what Claude prints today: a version that drops the
        // trailing zeros (`5m`, `1h 3m`), counts in days or writes a count in capitals
        // still reads.
        for status in [
            "5m · ↓ 6.0k tokens",
            "1h 3m · ↑ 96.4k tokens",
            "2d 3h 4m · ↓ 96.4k tokens",
            "1h 3m 12s · ↓ 1.2M tokens",
        ] {
            let screen = claude_agent_list(AGENTS_FOOTER, &[agent_row("builder", status)]);
            assert_subagents_running(&claude_read(&screen), status);
        }

        // The row the cursor is on starts with Claude's pointer, a nested agent's with
        // its branch, and the agent whose transcript is open has a filled bullet, the
        // same one `main` has (a dot, off macOS).
        for lead in ["❯ ◯", "  ├ ◯", "    └ ◯", "  ⏺", "  ●"] {
            let row = agent_row_as(lead, "builder", "4m 46s · ↓ 170.2k tokens");
            let screen = claude_agent_list(AGENTS_FOOTER, &[row]);
            assert_subagents_running(&claude_read(&screen), lead);
        }

        // Any of several agents counting is enough, wherever its row is in the list.
        let screen = claude_agent_list(
            AGENTS_FOOTER,
            &[
                agent_row("builder", "4m 46s · ↓ 170.2k tokens"),
                agent_row("reviewer", "idle"),
                agent_row("writer", "waiting"),
            ],
        );
        assert_subagents_running(&claude_read(&screen), "the first of three");

        // An agent that has only just started has counted no token, and its row ends in
        // its time alone; the test below has the shapes of that.
        let screen = claude_agent_list(AGENTS_FOOTER, &[agent_row("builder", "3s")]);
        assert_subagents_running(&claude_read(&screen), "no token counted yet");
    }

    #[test]
    fn claude_sub_agent_that_shows_its_time_alone_reads_working_in_the_list_under_main() {
        // Claude prints an agent's time with no tokens until it has counted the first,
        // so an agent that has only just started ends its row in its time. The row is
        // read in the list, under `main`, which Claude draws at the head of the list
        // whenever there is a row at all.
        for status in ["3s", "12s", "1m 0s", "4m 46s", "1h 3m 0s", "3s · 2 queued"] {
            let screen = claude_agent_list(AGENTS_FOOTER, &[agent_row("builder", status)]);
            assert_subagents_running(&claude_read(&screen), status);
        }

        // The row the cursor is on, a nested agent's and the open transcript's each
        // start differently, and so does `main`'s: the open transcript's bullet is
        // filled, the cursor's pointer comes first, and a list with rows scrolled out
        // of view says how many at the end of `main`'s row.
        for lead in ["❯ ◯", "  ├ ◯", "    └ ◯", "  ⏺", "  ●"] {
            let row = agent_row_as(lead, "builder", "3s");
            let screen = claude_agent_list(AGENTS_FOOTER, &[row]);
            assert_subagents_running(&claude_read(&screen), lead);
        }
        for main in [
            "  ⏺ main",
            "  ◯ main",
            "  ● main",
            "❯ ⏺ main",
            "❯ ◯ main",
            "  ⏺ main ↑ 2 more",
            "❯ ◯ main ↑ 12 more",
        ] {
            let screen =
                claude_agent_list_headed(AGENTS_FOOTER, main, &[agent_row("builder", "3s")]);
            assert_subagents_running(&claude_read(&screen), main);
        }

        // Wherever the row is in the list, after the rows that are not running.
        for (name, rows) in [
            (
                "the last of three",
                vec![
                    agent_row("reviewer", "idle"),
                    agent_row("writer", "waiting"),
                    agent_row("builder", "3s"),
                ],
            ),
            (
                "the middle of three",
                vec![
                    agent_row("reviewer", "idle"),
                    agent_row("builder", "3s"),
                    agent_row("writer", "waiting"),
                ],
            ),
            (
                "one that has counted and one that has not",
                vec![
                    agent_row("builder", "4m 46s · ↓ 170.2k tokens"),
                    agent_row("writer", "3s"),
                ],
            ),
        ] {
            let screen = claude_agent_list(AGENTS_FOOTER, &rows);
            assert_subagents_running(&claude_read(&screen), name);
        }
    }

    #[test]
    fn claude_line_ending_in_a_duration_outside_the_list_under_main_is_not_a_sub_agent() {
        // A time is not a row: any status line that ends in one would read as a running
        // agent if the rule took a time alone, so it is read only in the list under
        // `main`, and never from a line that is somewhere else under the box.
        let status = "  ⏺ Waiting on the build  12s\n";
        let idle_rows = format!(
            "{}\n{}\n",
            agent_row("reviewer", "idle"),
            agent_row("builder", "idle")
        );
        for (name, under) in [
            ("no list at all", status.to_string()),
            (
                "rows of idle agents with no main row",
                format!("{status}{idle_rows}"),
            ),
            (
                "above a list of idle rows, past a blank line",
                format!("{status}\n  ⏺ main\n{idle_rows}"),
            ),
            (
                "directly above a list of idle rows",
                format!("{status}  ⏺ main\n{idle_rows}"),
            ),
            (
                "under a list of idle rows, past a blank line",
                format!("\n  ⏺ main\n{idle_rows}\n{status}"),
            ),
        ] {
            let result = claude_read(&claude_box_then(AGENTS_FOOTER, &under));
            assert_eq!(result.state, AgentState::Idle, "{name}");
            assert_eq!(matched(&result), Some("live_prompt_box"), "{name}");
            assert!(result.visible_idle, "{name}");
            assert!(!subagent_rule_matched(&result), "{name}");
        }
    }

    #[test]
    fn claude_at_its_prompt_with_no_sub_agent_counting_reads_idle() {
        for (name, rows) in [
            (
                "every agent idle",
                vec![agent_row("reviewer", "idle"), agent_row("builder", "idle")],
            ),
            (
                "an idle agent with a message queued",
                vec![agent_row("builder", "idle · 1 queued")],
            ),
            ("an agent waiting", vec![agent_row("builder", "waiting")]),
            (
                "a plan waiting to be approved",
                vec![agent_row("planner", "awaiting approval")],
            ),
            (
                "the row that stands for the idle ones",
                vec!["  ◯ 3 idle agents".to_string()],
            ),
            ("no agents at all", vec![]),
        ] {
            let result = claude_read(&claude_agent_list(AGENTS_FOOTER, &rows));
            assert_eq!(result.state, AgentState::Idle, "{name}");
            assert_eq!(matched(&result), Some("live_prompt_box"), "{name}");
            assert!(result.visible_idle, "{name}");
            assert!(!subagent_rule_matched(&result), "{name}");
        }
    }

    #[test]
    fn claude_background_shell_in_the_footer_is_not_a_sub_agent() {
        // herdr reads a background shell at an idle prompt as idle, and the rule for
        // agents has no agent to find: the footer's `1 shell` and its `for agents` hint
        // are not a row.
        for footer in [AGENTS_FOOTER, "⏵⏵ auto mode on · 1 shell · ← for agents"] {
            let result = claude_read(&claude_agent_list(footer, &[]));
            assert_eq!(result.state, AgentState::Idle, "{footer}");
            assert_eq!(matched(&result), Some("live_prompt_box"), "{footer}");
            assert!(!subagent_rule_matched(&result), "{footer}");
        }
    }

    #[test]
    fn claude_sub_agent_rows_above_the_prompt_box_are_not_agents() {
        let running = agent_row("builder", "4m 46s · ↓ 170.2k tokens");
        let quoted = claude_agent_list(AGENTS_FOOTER, std::slice::from_ref(&running));
        let plain = claude_agent_list("? for shortcuts", &[]);

        // A transcript that quotes a footer, a user pasting one, over a prompt box whose
        // own footer holds no agent.
        let transcript = format!("{quoted}\n⏺ That is the footer from my last session.\n\n{plain}");
        let result = claude_read(&transcript);
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("live_prompt_box"));
        assert!(!subagent_rule_matched(&result));

        // The same rows pasted into the prompt box: a draft, between the box's own rules.
        let rule = "─".repeat(100);
        let draft = format!(
            "⏺ Ready.\n\n{rule}\n❯ what does this mean?\n{running}\n{rule}\n  ? for shortcuts\n"
        );
        let result = claude_read(&draft);
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("live_prompt_box"));
        assert!(!subagent_rule_matched(&result));
    }

    #[test]
    fn claude_capture_with_no_horizontal_rule_is_read_whole() {
        // With no rule to cut at the region is the whole capture, so the list is read
        // wherever it stands, and so is a footer quoted in the transcript: only what
        // stands above a rule is left out.
        let list = format!(
            "  ⏺ main\n{}\n{}\n",
            agent_row("reviewer", "idle"),
            agent_row("builder", "4m 46s · ↓ 170.2k tokens")
        );
        let footer = format!("  {AGENTS_FOOTER}\n\n");

        // The list at the foot of a capture that shows no rule, as after a scroll.
        let screen = format!("⏺ Both agents are on it.\n\n{footer}{list}");
        assert_subagents_running(&claude_read(&screen), "the list at the foot");

        // The list with the transcript after it.
        let screen = format!("{footer}{list}\n⏺ That is the footer from my last session.\n");
        assert_subagents_running(&claude_read(&screen), "the list, then the transcript");

        // A rule under the list puts it above the region, and nothing is read.
        let screen = format!("{footer}{list}{}\n", "─".repeat(100));
        let result = claude_read(&screen);
        assert!(!subagent_rule_matched(&result), "a rule under the list");
        assert_eq!(result.state, AgentState::Idle);
    }

    #[test]
    fn claude_blocker_beside_a_running_sub_agent_still_reads_blocked() {
        let rule = "─".repeat(100);
        let footer = format!("  {AGENTS_FOOTER}\n\n  ⏺ main\n");
        let running = agent_row("builder", "4m 46s · ↓ 170.2k tokens");

        // A permission dialog with the agent list drawn under it: the dialog's own
        // rule is the last, so the whole dialog is in the text the rule reads, and the
        // phrases that mark it keep the rule from matching.
        let permission = format!(
            "{rule}\n Bash command\n\n   curl -sS https://example.com\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again for: curl *\n   3. No\n\n Esc to cancel · Tab to amend · ctrl+e to explain\n\n{footer}{running}\n"
        );
        let result = claude_read(&permission);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("bash_permission_prompt"));
        assert!(!subagent_rule_matched(&result));

        // A question in the main, with the list under it: a blocker above the rule's
        // own priority.
        let question = format!(
            "{rule}\n Which parser should the builder use?\n\n ❯ 1. The old one\n   2. The new one\n\n Enter to select · ↑/↓ to navigate · Esc to cancel\n\n{footer}{running}\n"
        );
        let result = claude_read(&question);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("live_blocked_form"));
        assert!(result.visible_blocker);

        // A dialog drawn under the prompt box and its list, as Claude draws one that
        // does not take the prompt away: the rows are above its rule, and it is what
        // stands under the last one.
        let below = format!(
            "{}{rule}\n Which parser should the builder use?\n\n ❯ 1. The old one\n   2. The new one\n\n Enter to confirm · Esc to cancel\n",
            claude_agent_list(AGENTS_FOOTER, std::slice::from_ref(&running))
        );
        let result = claude_read(&below);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("live_blocked_form"));
        assert!(!subagent_rule_matched(&result));

        // The dialogs that have no "Esc to cancel" footer for the permission rules to
        // read, with the list drawn under each. Only the legacy blocker (300) reads them,
        // and it can only while this rule stands aside, so the rule carries the phrases
        // the legacy blocker keys on.
        for (name, dialog) in CLAUDE_DIALOGS_WITHOUT_AN_ESC_FOOTER {
            let screen = format!("{rule}\n{dialog}\n{footer}{running}\n");
            let result = claude_read(&screen);
            assert_eq!(result.state, AgentState::Blocked, "{name}");
            assert_eq!(matched(&result), Some("legacy_no_prompt_blocker"), "{name}");
            assert!(!subagent_rule_matched(&result), "{name}");
        }
    }

    #[test]
    fn claude_dialog_beside_a_background_shell_still_reads_blocked() {
        // The shell rule (965) stands over the legacy blocker (300) as the sub-agent rule
        // does, and the dialogs that only the legacy blocker reads must not read working
        // for a shell that runs on. The shell shows in two places: the completion line
        // of the turn above a dialog that took the prompt box away, and the footer under
        // it.
        let rule = "─".repeat(100);
        let with_dialog = |dialog: &str| {
            [
                (
                    "the completion line above",
                    format!(
                        "⏺ started\n\n✻ Cogitated for 18s · 1 shell still running\n\n{rule}\n{dialog}"
                    ),
                ),
                (
                    "the footer under",
                    format!(
                        "⏺ started\n\n{rule}\n{dialog}\n  -- INSERT -- ⏵⏵ auto mode on · 1 shell\n"
                    ),
                ),
            ]
        };

        // Both places read as a shell running when there is nothing to answer.
        for (place, screen) in with_dialog(" Working on the parser\n") {
            let result = claude_read(&screen);
            assert_eq!(result.state, AgentState::Working, "{place}");
            assert_eq!(
                matched(&result),
                Some("taw_background_shell_working"),
                "{place}"
            );
        }

        for (name, dialog) in CLAUDE_DIALOGS_WITHOUT_AN_ESC_FOOTER {
            for (place, screen) in with_dialog(dialog) {
                let result = claude_read(&screen);
                assert_eq!(result.state, AgentState::Blocked, "{name}, {place}");
                assert_eq!(
                    matched(&result),
                    Some("legacy_no_prompt_blocker"),
                    "{name}, {place}"
                );
                assert!(
                    !taw_rule_matched(&result, "taw_background_shell_working"),
                    "{name}, {place}"
                );
            }
        }
    }

    #[test]
    fn claude_turn_in_progress_beside_a_running_sub_agent_is_read_by_its_own_rule() {
        // The spinner line is the turn, and the turn rules sit over the sub-agent rule:
        // the state is the same, and `agent explain` says the turn is why.
        let rows = [agent_row("builder", "4m 46s · ↓ 170.2k tokens")];
        let screen = format!(
            "✽ Combobulating…\n\n{}",
            claude_agent_list(AGENTS_FOOTER, &rows)
        );
        let result = claude_read(&screen);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("live_turn_working"));
        assert!(result.visible_working);
        assert!(subagent_rule_matched(&result), "both rules see the screen");
    }

    // -----------------------------------------------------------------------
    // Grok 2026.10.05.1 (herdr master fabcab108f; the tests below are from master and taw's).
    // -----------------------------------------------------------------------

    // Follows herdr's src/detect/manifest/tests.rs. Differences: `osc_explain(Agent::Grok, ..)` becomes taw's `osc_explain`
    // over the bundled "grok" manifest.
    #[test]
    fn grok_osc_activity_does_not_depend_on_configurable_title_text() {
        for (title, progress, expected_state, expected_rule) in [
            (
                "project · session · id",
                "4;0;0",
                AgentState::Idle,
                "osc_progress_idle",
            ),
            (
                "project · session · id",
                "",
                AgentState::Idle,
                "prompt_hints_idle",
            ),
            ("grok", "", AgentState::Idle, "osc_title_idle"),
            (
                "⠋ - Waiting for response… - project",
                "",
                AgentState::Working,
                "osc_title_working",
            ),
            (
                "project - ⠹ - session",
                "4;0;0",
                AgentState::Working,
                "osc_title_working",
            ),
            (
                "project · session · id",
                "4;1;-1",
                AgentState::Working,
                "osc_progress_working",
            ),
            (
                "grok",
                "4;1;-1",
                AgentState::Working,
                "osc_progress_working",
            ),
            (
                "⚠ Action Required - project",
                "4;1;-1",
                AgentState::Blocked,
                "osc_title_blocked",
            ),
        ] {
            let result = osc_explain(
                "grok",
                "Shift+Tab:mode │ Ctrl+.:shortcuts\n",
                title,
                progress,
            );
            assert_eq!(
                result.state, expected_state,
                "title={title}, progress={progress}"
            );
            assert_eq!(
                matched(&result),
                Some(expected_rule),
                "title={title}, progress={progress}",
            );
        }
    }

    // Follows herdr's src/detect/manifest/tests.rs. Differences: as above.
    #[test]
    fn grok_visible_activity_outranks_idle_osc_signals() {
        for (screen, expected_rule) in [
            (
                "⠴ Sleep for 8 … 1.9s 5.0s ⇣19.5k [↓][stop]\n",
                "spinner_status_working",
            ),
            (
                "Shift+Tab:mode │ Ctrl+c:cancel │ Ctrl+.:shortcuts\n",
                "esc_cancel_hints_working",
            ),
            (
                "Shift+Tab:mode │ Esc:cancel │ Ctrl+.:shortcuts\n",
                "esc_cancel_hints_working",
            ),
        ] {
            for progress in ["", "4;0;0"] {
                let result = osc_explain("grok", screen, "grok", progress);
                assert_eq!(result.state, AgentState::Working, "screen={screen}");
                assert_eq!(matched(&result), Some(expected_rule));
            }
            let blocked = osc_explain("grok", screen, "⚠ Action Required - grok", "4;1;-1");
            assert_eq!(blocked.state, AgentState::Blocked);
        }
    }

    // Written for taw from the manifest's own comment: herdr removed Grok's two background
    // rules on 2026-10-05 (fabcab108f, #4959), so an agent back at its prompt with
    // background commands, monitors, loops or subagents running reads idle, as Claude's
    // does. The positive control comes first in each pass: the same OSC signals with a
    // live turn's footer read working, so the idle verdicts are not an engine that reads
    // nothing.
    #[test]
    fn grok_background_work_at_the_prompt_reads_idle() {
        let working = osc_explain(
            "grok",
            "Shift+Tab:mode │ Esc:cancel │ Ctrl+.:shortcuts\n",
            "project · session",
            "4;0;0",
        );
        assert_eq!(working.state, AgentState::Working);
        assert_eq!(matched(&working), Some("esc_cancel_hints_working"));

        let body = "Worked for 3.9s\n\nShift+Tab:mode │ Ctrl+.:shortcuts\n";
        for screen in [
            "◎ 1 command still running\n".to_string(),
            "○ 1 command still running\n".to_string(),
            "◉ 1 command still running\n".to_string(),
            "◎ 2 commands · 1 subagent still running · send a message to interrupt\n".to_string(),
            "1 command still running\n".to_string(),
            "◎ 0 commands still running\n".to_string(),
            format!("grok  project ⋅ 2 │ main\n{body}"),
            format!("grok  project : 12 │ main\n{body}"),
        ] {
            for (title, progress) in [("project · session", "4;0;0"), ("", "")] {
                let result = osc_explain("grok", &screen, title, progress);
                assert_eq!(result.state, AgentState::Idle, "{screen} under {title:?}");
                assert!(!result.visible_working, "{screen} under {title:?}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // The 2026-10 manifest refresh (herdr master 5608de0608): claude 2026.10.11.1, codex
    // 2026.10.11.1 and agy 2026.10.05.1. Written for taw from each rule's own strings;
    // herdr's own tests for these commits are the two synthetic engine tests above.
    // -----------------------------------------------------------------------

    // A Claude session that has a name draws the name's divider under a question's footer, so
    // the footer is no longer the last line (herdr #4573).
    #[test]
    fn claude_question_form_in_a_named_session_reads_blocked() {
        let footer = "Enter to select · ↑/↓ to navigate · Esc to cancel";
        let divider = "──────────────────────────────── my-session ──";
        let panel = "  3 tasks (1 done, 1 in progress, 1 open)\n  ✔ read\n  ◼ edit\n  ◻ test\n";
        for tail in [
            divider.to_string(),
            format!("{divider}\n{panel}"),
            format!("{divider}\n{panel}  … +4 pending\n"),
        ] {
            let screen = format!("Which file?\n  1. a.rs\n  2. b.rs\n\n{footer}\n{tail}\n");
            let result = explain("claude", &screen);
            assert_eq!(result.state, AgentState::Blocked, "{screen}");
            assert_eq!(
                matched(&result),
                Some("live_blocked_form_named_session"),
                "{screen}"
            );
            assert!(result.visible_blocker, "{screen}");

            // Anything newer than the divider or its panel makes the form history.
            let later = format!("{screen}\n> next prompt\n");
            let result = explain("claude", &later);
            assert_ne!(
                matched(&result),
                Some("live_blocked_form_named_session"),
                "{later}"
            );
        }
    }

    #[test]
    fn codex_update_picker_of_0_159_reads_blocked_and_the_history_notice_does_not() {
        let picker = "  Update available · 0.158.0 -> 0.159.0\n\n\
            › 1. Update now\n\
              2. Skip\n\n\
            enter continue · esc skip\n";
        let result = explain("codex", picker);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("startup_update"));
        assert!(result.visible_blocker);

        // The passive notice in the transcript has the heading and not the footer.
        let notice = "  Update available · 0.158.0 -> 0.159.0\n\n› \n";
        let result = explain("codex", notice);
        assert_ne!(matched(&result), Some("startup_update"));
        assert_ne!(result.state, AgentState::Blocked);

        // Neither gate alone is enough: a footer without the heading is not the picker.
        let footer_only = "something else\n\nenter continue · esc skip\n";
        assert_ne!(
            matched(&explain("codex", footer_only)),
            Some("startup_update")
        );
    }

    #[test]
    fn agy_dialogs_and_turns_read_from_their_footers() {
        for (screen, rule) in [
            (
                "Allow this command?\n↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\n  esc to cancel\n",
                "permission_prompt",
            ),
            (
                "Which option?\n↑/↓ Navigate · enter Select · esc Skip\n  esc to cancel\n",
                "question_prompt",
            ),
        ] {
            let result = explain("agy", screen);
            assert_eq!(result.state, AgentState::Blocked, "{screen}");
            assert_eq!(matched(&result), Some(rule), "{screen}");
        }

        let working = "⠋ Thinking about it\n\n╭──────╮\n│ > │\n╰──────╯\n─── esc to cancel\n";
        let result = explain("agy", working);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("esc_cancel_footer_working"));

        // Back at the prompt with background tasks running reads idle: the manifest has no
        // rule for the task count or its panel.
        let background = "╭──────╮\n│ > │\n╰──────╯\n? for shortcuts · 2 tasks\n";
        let result = explain("agy", background);
        assert_eq!(result.state, AgentState::Idle);
        assert!(!result.visible_working);
    }

    #[test]
    fn agy_trust_dialog_reads_blocked_from_its_question_choice_and_hint() {
        let dialog = "Do you trust the contents of this project?\n\n\
            › Yes, I trust this folder\n\
              No, exit\n\n\
            ↑/↓ Navigate · enter Confirm\n";
        let result = explain("agy", dialog);
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("trust_prompt"));
        assert!(result.visible_blocker);

        // The rule wants all three strings: the hint row alone is not the dialog.
        let hint_only = "↑/↓ Navigate · enter Confirm\n";
        assert_ne!(matched(&explain("agy", hint_only)), Some("trust_prompt"));
    }

    #[test]
    fn agy_pre_1_1_28_permission_dialog_reads_blocked_by_either_closing_line() {
        for closing in ["Do you want to proceed?", "tab Amend · edit command"] {
            let screen = format!("Requesting permission for: rm -rf build\n\n{closing}\n");
            let result = explain("agy", &screen);
            assert_eq!(result.state, AgentState::Blocked, "{closing}");
            assert_eq!(
                matched(&result),
                Some("legacy_permission_prompt"),
                "{closing}"
            );
            assert!(result.visible_blocker, "{closing}");
        }

        // The heading with neither closing line is no dialog.
        let heading_only = "Requesting permission for: rm -rf build\n";
        assert_ne!(
            matched(&explain("agy", heading_only)),
            Some("legacy_permission_prompt")
        );
    }

    #[test]
    fn agy_braille_spinner_line_reads_working_without_the_cancel_footer() {
        let screen = "⠙ Reading the repository\n\n╭──────╮\n│ > │\n╰──────╯\n? for shortcuts\n";
        let result = explain("agy", screen);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("spinner_working"));
        assert!(result.visible_working);

        // Braille alone, with no word after it, is not the spinner line.
        let bare = "⠙\n\n╭──────╮\n│ > │\n╰──────╯\n? for shortcuts\n";
        assert_ne!(matched(&explain("agy", bare)), Some("spinner_working"));
    }

    // -----------------------------------------------------------------------
    // Kiro 2026.09.19.1 (herdr v0.9.2): state comes from the live controls at the
    // bottom of the screen and from OSC. Written for taw from each rule's own strings;
    // herdr has no Rust test for them.
    // -----------------------------------------------------------------------

    #[test]
    fn kiro_live_prompt_reads_idle_only_at_the_bottom_of_the_screen() {
        for prompt in [
            "› Ask a question or describe a task",
            "> ask a question or describe a task ↵",
            "  ›  Ask a question or describe a task   enter  ",
        ] {
            let screen = format!("Done. Anything else?\n\n{prompt}\n");
            let result = explain("kiro", &screen);
            assert_eq!(result.state, AgentState::Idle, "{prompt}");
            assert_eq!(matched(&result), Some("live_prompt_idle"), "{prompt}");
            assert!(result.visible_idle, "{prompt}");
        }

        // The same words in the transcript, with more than four lines under them,
        // are not the live prompt.
        let scrolled = "› Ask a question or describe a task\nline 1\nline 2\nline 3\nline 4\n";
        assert_idle_fallback(&explain("kiro", scrolled), "scrolled off");
    }

    /// One choice per line, the cursor glyph on the chosen one and two spaces before
    /// the rest, the way Kiro draws a list.
    fn kiro_choices(options: &[&str], cursor: usize, glyph: char) -> String {
        options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                if index == cursor {
                    format!("{glyph} {option}\n")
                } else {
                    format!("  {option}\n")
                }
            })
            .collect()
    }

    #[test]
    fn kiro_tool_approval_needs_its_footer_and_a_whole_choice_list() {
        // Each choice list is three rules' worth of lines that must all be on screen,
        // one of them carrying the cursor, so walk the cursor down every list.
        let lists: [&[&str]; 3] = [
            &["Allow", "Always allow", "Deny", "Always deny"],
            &[
                "Yes, single permission",
                "Trust, always allow in this session",
                "No (tab to edit)",
            ],
            &["Trust this tool", "Entire tool"],
        ];
        let footers = [
            "esc to close · ↑/↓ to navigate",
            "esc to close · ↑/↓ to navigate · enter to select · tab to edit",
            "esc to close · enter to see more options",
        ];

        for options in lists {
            for cursor in 0..options.len() {
                for glyph in ['>', '❯'] {
                    for footer in footers {
                        let choices = kiro_choices(options, cursor, glyph);
                        let screen = format!("fs_write wants to run\n\n{choices}\n{footer}\n");
                        let result = explain("kiro", &screen);
                        assert_eq!(result.state, AgentState::Blocked, "{screen}");
                        assert_eq!(matched(&result), Some("tool_approval"), "{screen}");
                        assert!(result.visible_blocker, "{screen}");
                    }
                }
            }
        }

        // A footer with no choice list, a choice list with no footer, and a list with
        // one of its lines missing are all text.
        let footer_only = "fs_write wants to run\n\nesc to close · ↑/↓ to navigate\n";
        assert_idle_fallback(&explain("kiro", footer_only), "footer only");
        let choices = kiro_choices(lists[0], 0, '>');
        let choices_only = format!("fs_write wants to run\n\n{choices}");
        assert_idle_fallback(&explain("kiro", &choices_only), "choices only");
        let short = kiro_choices(&lists[0][..3], 0, '>');
        let short_list =
            format!("fs_write wants to run\n\n{short}\nesc to close · ↑/↓ to navigate\n");
        assert_idle_fallback(
            &explain("kiro", &short_list),
            "a list missing its last line",
        );
    }

    #[test]
    fn kiro_modify_request_editor_is_blocked() {
        let screen = "────────────────────────────────\n\
            fs_write requires approval · modify request\n\
            \n\
            › {\"path\": \"/tmp/notes.txt\"}\n\
            ────────────────────────────────\n\
            esc to close\n";
        let result = explain("kiro", screen);

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("tool_approval_edit"));
        assert!(result.visible_blocker);

        // The editor is the last thing drawn: anything after its footer means it closed.
        let closed = format!("{screen}› Ask a question or describe a task\n");
        assert_ne!(
            matched(&explain("kiro", &closed)),
            Some("tool_approval_edit")
        );
    }

    #[test]
    fn kiro_crew_approval_needs_all_four_of_its_controls() {
        let controls = [
            "Tool approval",
            "Approve all pending",
            "Configure individually (Agent Monitor)",
            "Exit (cancel subagents)",
        ];
        let screen = format!("{}\n", controls.join("\n"));
        let result = explain("kiro", &screen);

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("crew_approval"));
        assert!(result.visible_blocker);

        for missing in 0..controls.len() {
            let partial: Vec<&str> = controls
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != missing)
                .map(|(_, control)| *control)
                .collect();
            let result = explain("kiro", &format!("{}\n", partial.join("\n")));
            assert_ne!(
                matched(&result),
                Some("crew_approval"),
                "without {}",
                controls[missing]
            );
        }
    }

    #[test]
    fn kiro_question_panel_is_blocked() {
        let screen = "Which database should the service use?\n\n\
            › Postgres\n  SQLite\n\n\
            ↑/↓ to navigate · enter to submit · esc to cancel\n";
        let result = explain("kiro", screen);

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("question_panel"));
        assert!(result.visible_blocker);
    }

    #[test]
    fn kiro_working_footer_needs_all_three_of_its_hints() {
        let screen = "Reading files\n\n\
            Kiro is working · type to steer · ctrl+s to queue\n";
        let result = explain("kiro", screen);

        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("live_working_footer"));
        assert!(result.visible_working);

        let partial = "Kiro is working · type to steer\n";
        assert_idle_fallback(&explain("kiro", partial), "two of three hints");
    }

    #[test]
    fn kiro_osc_title_spinner_and_progress_read_working() {
        for frame in ['◐', '◓', '◑', '◒', '/', '|', '\\', '-'] {
            let title = format!("{frame} Kiro: refactor the parser");
            let result = osc_explain("kiro", "", &title, "");
            assert_eq!(result.state, AgentState::Working, "frame {frame}");
            assert_eq!(matched(&result), Some("osc_title_working"), "frame {frame}");
            assert!(result.visible_working, "frame {frame}");
        }
        // A title with no spinner frame is not activity.
        assert_idle_fallback(
            &osc_explain("kiro", "", "Kiro: refactor the parser", ""),
            "title",
        );

        for progress in ["4;3;", "4;3"] {
            let result = osc_explain("kiro", "", "Kiro", progress);
            assert_eq!(result.state, AgentState::Working, "{progress}");
            assert_eq!(matched(&result), Some("osc_progress_working"), "{progress}");
            assert!(result.visible_working, "{progress}");
        }
        assert_idle_fallback(&osc_explain("kiro", "", "Kiro", "4;0;0"), "progress 4;0;0");
    }

    #[test]
    fn kiro_blocked_screens_outrank_working_signals() {
        let approval = "fs_write wants to run\n\n> Allow\n  Always allow\n  Deny\n  Always deny\n\n\
            esc to close · ↑/↓ to navigate\n";
        let result = osc_explain("kiro", approval, "◐ Kiro: refactor", "4;3;");
        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("tool_approval"));

        // And the empty prompt outranks a spinner title left behind.
        let prompt = "› Ask a question or describe a task\n";
        let result = osc_explain("kiro", prompt, "◐ Kiro: refactor", "");
        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("live_prompt_idle"));
    }

    // -----------------------------------------------------------------------
    // Cline 2026.09.11.1 and Pi 2026.09.14.1 (herdr v0.9.1), and Pi 2026.10.01.1 (herdr
    // master). herdr has no Rust test for these rules; each screen is built from the
    // rule's own strings. Cline reads working for any screen no rule takes
    // (`default_cline_working`), so a screen that is not the state under test shows up
    // as that rule, not as idle.
    // -----------------------------------------------------------------------

    #[test]
    fn cline_inline_tool_permission_is_blocked() {
        let screen = "Cline needs permission\n\nApprove tool call?\n[y] Approve   [n] Deny\n";
        let result = explain("cline", screen);

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("inline_tool_permission"));
        assert!(result.visible_blocker);

        // The prompt needs all four of its strings.
        for missing in [
            "Cline needs permission",
            "Approve tool call?",
            "[y] Approve",
            "[n] Deny",
        ] {
            let partial = explain("cline", &screen.replace(missing, ""));
            assert_eq!(
                matched(&partial),
                Some("default_cline_working"),
                "without {missing}"
            );
        }

        // It is the live prompt at the bottom of the screen: 16 non-empty lines of
        // later output push it out of the region.
        let scrolled = format!("{screen}{}", "output\n".repeat(16));
        assert_eq!(
            matched(&explain("cline", &scrolled)),
            Some("default_cline_working")
        );

        // A spinner still on screen does not hide it.
        let spinning = format!("⠋ Running\n{screen}");
        assert_eq!(
            matched(&explain("cline", &spinning)),
            Some("inline_tool_permission")
        );
    }

    #[test]
    fn cline_inline_question_is_blocked() {
        let screen = "Cline is asking a question\n> Use the existing schema\n(Tab)  Shift+Tab\n";
        let result = explain("cline", screen);

        assert_eq!(result.state, AgentState::Blocked);
        assert_eq!(matched(&result), Some("inline_question"));
        assert!(result.visible_blocker);

        // The heading, an answer line and both key hints are all needed.
        for partial in [
            "> Use the existing schema\n(Tab)  Shift+Tab\n",
            "Cline is asking a question\n(Tab)  Shift+Tab\n",
            "Cline is asking a question\n> Use the existing schema\n",
            "Cline is asking a question\n> Use the existing schema\n(Tab)\n",
        ] {
            assert_eq!(
                matched(&explain("cline", partial)),
                Some("default_cline_working"),
                "{partial}"
            );
        }
    }

    #[test]
    fn cline_braille_turn_is_working() {
        // The lowest and highest frames of the braille block, and a spinner between.
        for frame in ['\u{2801}', '⠋', '\u{28FF}'] {
            let screen = format!("{frame} Reading files\n\n❯\n");
            let result = explain("cline", &screen);
            assert_eq!(result.state, AgentState::Working, "{frame}");
            assert_eq!(matched(&result), Some("active_turn"), "{frame}");
            assert!(result.visible_working, "{frame}");
        }
        let thinking = explain("cline", "Thinking... (esc to cancel)\n");
        assert_eq!(thinking.state, AgentState::Working);
        assert_eq!(matched(&thinking), Some("active_turn"));

        // U+2800, the blank braille cell, is not a spinner frame.
        let blank = explain("cline", "\u{2800} Reading files\n");
        assert_eq!(matched(&blank), Some("default_cline_working"));

        // A permission prompt outranks the spinner above it.
        let prompt =
            "⠋ Running\nCline needs permission\nApprove tool call?\n[y] Approve\n[n] Deny\n";
        assert_eq!(
            matched(&explain("cline", prompt)),
            Some("inline_tool_permission")
        );
    }

    #[test]
    fn cline_composer_is_idle() {
        let rule = "────────────────────────────────";
        let screen = format!("{rule}\n❯\n{rule}\n(Tab)  Shift+Tab\n");
        let result = explain("cline", &screen);

        assert_eq!(result.state, AgentState::Idle);
        assert_eq!(matched(&result), Some("composer_idle"));
        assert!(result.visible_idle);

        // Typed text does not make it any less the composer.
        let typing = format!("{rule}\n❯ fix the failing test\n{rule}\n(Tab)  Shift+Tab\n");
        assert_eq!(matched(&explain("cline", &typing)), Some("composer_idle"));

        // Without the rule and the key hints around it, `❯` is text.
        let bare = explain("cline", "❯ fix the failing test\n");
        assert_eq!(matched(&bare), Some("default_cline_working"));

        // A running turn above the composer is working, not idle.
        let running = format!("⠋ Reading files\n{screen}");
        let result = explain("cline", &running);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched(&result), Some("active_turn"));
    }

    #[test]
    fn pi_working_border_is_working() {
        for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            let screen = format!("── {frame} Working ─────────────────\n> \n");
            let result = explain("pi", &screen);
            assert_eq!(result.state, AgentState::Working, "{frame}");
            assert_eq!(matched(&result), Some("working_border"), "{frame}");
            assert!(result.visible_working, "{frame}");
        }

        // The literal the older chrome prints is still a working signal.
        let literal = explain("pi", "Working...\n");
        assert_eq!(literal.state, AgentState::Working);
        assert_eq!(matched(&literal), Some("working_literal"));

        // Not the border: no spinner, no trailing rule, or a line that is not all border.
        for screen in [
            "── Working ─────────────────\n",
            "── ⠋ Working\n",
            "── ⠋ Working ── and more\n",
            "old text ── ⠋ Working ─────\n",
        ] {
            assert_idle_fallback(&explain("pi", screen), screen);
        }

        // It is live chrome near the bottom: 12 non-empty lines of output push it out.
        let scrolled = format!("── ⠋ Working ─────\n{}", "output\n".repeat(12));
        assert_idle_fallback(&explain("pi", &scrolled), "scrolled off");
    }

    // Pi 2026.10.01.1: a bare spinner line is a turn as well. Written for taw from the
    // rules' own strings; herdr has no Rust test for it.
    #[test]
    fn pi_bare_spinner_line_is_working() {
        for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            let screen = format!("{frame} Working\n\n> \n");
            let result = explain("pi", &screen);
            assert_eq!(result.state, AgentState::Working, "{frame}");
            // Both of Pi's working rules take the line, at the same priority; the first
            // in the file is the verdict.
            assert_eq!(matched(&result), Some("working_literal"), "{frame}");
            assert!(result.visible_working, "{frame}");
            for rule in ["working_literal", "working_border"] {
                assert!(rule_matched(&result, rule), "{rule} on {frame}");
            }
        }

        // It is the whole line: more words, no spinner, a spinner glued to the word, an
        // indent, or the older border without its rule.
        for screen in [
            "⠋ Working on it\n",
            "⠋ Working…\n",
            "Working\n",
            "⠋Working\n",
            " ⠋ Working\n",
            "\u{2800} Working\n",
            "── ⠋ Working\n",
        ] {
            assert_idle_fallback(&explain("pi", screen), screen);
        }
    }

    // -----------------------------------------------------------------------
    // Letta 2026.08.24.1 (herdr v0.9.1).
    // -----------------------------------------------------------------------

    // Follows herdr's src/detect/mod.rs. Differences:
    // `manifest::explain(Agent::Letta, ..)` and `detect_state(Some(Agent::Letta), ..)`
    // become taw's `explain` over the bundled "letta" manifest.
    #[test]
    fn letta_manifest_detects_observed_working_and_idle_chrome() {
        let empty = explain("letta", "");
        assert_eq!(empty.state, AgentState::Unknown);
        assert_eq!(matched(&empty), Some("no_live_state_evidence"));
        for screen in [
            "✻ Thinking…\nTutor is reflecting… (esc to interrupt · 2m 3s)",
            "My Tutor is thinking about thinking… (esc to interrupt · 3s)",
            "Tutor is calibrating… (interrupting)",
            "• Run Compile the integration\n└ Running... (1s)",
        ] {
            assert_eq!(explain("letta", screen).state, AgentState::Working);
        }
        assert_eq!(
            explain(
                "letta",
                "────────────────\n› Try \"debug this error\"\n────────────────\nTutor · No model selected"
            )
            .state,
            AgentState::Idle
        );
        assert_eq!(
            explain(
                "letta",
                "────────────────\n› explain this code\n────────────────\nTutor · No model selected"
            )
            .state,
            AgentState::Unknown
        );
        let selector = explain(
            "letta",
            "8 pinned agents available.\n\n> Resume Bob (pinned)\n  View all 8 profiles\n  Create a new agent (--new)\n\n  ↑↓ navigate · Enter select · Esc exit",
        );
        assert_eq!(selector.state, AgentState::Unknown);
        assert_eq!(matched(&selector), Some("profile_selector"));
    }

    // Follows herdr's src/detect/mod.rs. Differences:
    // `detect_agent_with_osc(Some(Agent::Letta), ..)` becomes taw's `osc_explain`.
    #[test]
    fn letta_manifest_uses_osc_activity_and_approval_signals() {
        let idle_screen =
            "────────────────\n› Try \"debug this error\"\n────────────────\nTutor · GPT-5.5";

        for title in ["⠋ Tutor", "project | ⠏ Tutor"] {
            let detection = osc_explain("letta", idle_screen, title, "");
            assert_eq!(detection.state, AgentState::Working);
            assert!(detection.visible_working);
        }

        for title in [
            "[ ! ] Action Required | Tutor",
            "[ . ] Action Required | Tutor",
        ] {
            let detection = osc_explain("letta", idle_screen, title, "");
            assert_eq!(detection.state, AgentState::Blocked);
            assert!(detection.visible_blocker);
        }

        let detection = osc_explain("letta", idle_screen, "Tutor", "4;3;0");
        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    // Follows herdr's src/detect/mod.rs. Differences: as above.
    #[test]
    fn letta_manifest_detects_observed_command_approval() {
        let approval = r#"✻ Thinking…

────────────────────────────────────────────────────────────────
Run this command?

  $ rm -f /var/tmp/herdr-blocked-capture-never-created

❯ 1. Yes
  2. No, and tell Letta Code what to do differently

Enter to select · Esc to cancel"#;
        assert_eq!(explain("letta", approval).state, AgentState::Blocked);
        assert_eq!(
            explain(
                "letta",
                "The user asked: Run this command?\n›\n────\nTutor · No model selected"
            )
            .state,
            AgentState::Idle
        );
    }

    // -----------------------------------------------------------------------
    // RuleSource / append_supplement.
    // -----------------------------------------------------------------------

    #[test]
    fn every_rule_from_parse_manifest_is_reported_as_manifest_sourced() {
        let manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "plain"
state = "idle"
contains = ["ready"]
"#,
        ))
        .unwrap();

        let evaluation = on(&manifest, "ready");
        assert_eq!(evaluation.evaluated_rules[0].source, RuleSource::Manifest);
        assert_eq!(
            evaluation.matched_rule.unwrap().source,
            RuleSource::Manifest
        );
    }

    #[test]
    fn append_supplement_tags_its_rules_taw_and_lets_them_be_evaluated() {
        let mut manifest = parse(&rules_manifest(
            r#"
[[rules]]
id = "base_idle"
state = "idle"
priority = 100
contains = ["ready"]
"#,
        ))
        .unwrap();

        let appended = manifest
            .append_supplement(
                r#"
[[rules]]
id = "taw_extra"
state = "working"
priority = 200
contains = ["extra"]
"#,
            )
            .unwrap();
        assert_eq!(appended, 1);

        let evaluation = on(&manifest, "ready extra");
        assert_eq!(evaluation.state, AgentState::Working);
        let matched_rule = evaluation.matched_rule.unwrap();
        assert_eq!(matched_rule.id, "taw_extra");
        assert_eq!(matched_rule.source, RuleSource::Taw);

        let base_rule = evaluation
            .evaluated_rules
            .iter()
            .find(|rule| rule.id == "base_idle")
            .unwrap();
        assert_eq!(base_rule.source, RuleSource::Manifest);
    }

    #[test]
    fn append_supplement_rejects_a_document_with_a_manifest_header() {
        let mut manifest = bundled("codex");
        let err = manifest
            .append_supplement(
                r#"
id = "codex"

[[rules]]
id = "taw_extra"
state = "working"
contains = ["extra"]
"#,
            )
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("unknown"));
    }

    #[test]
    fn append_supplement_reuses_manifest_rule_validation() {
        let mut manifest = bundled("codex");
        assert!(
            manifest
                .append_supplement(
                    r#"
[[rules]]
id = "taw_bad_regex"
state = "working"
regex = ["["]
"#,
                )
                .is_err()
        );
        assert!(
            manifest
                .append_supplement(
                    r#"
[[rules]]
id = "taw_bad_region"
state = "working"
region = "not_a_real_region"
contains = ["x"]
"#,
                )
                .is_err()
        );
    }

    #[test]
    fn rule_source_serialises_lowercase() {
        assert_eq!(
            serde_json::to_string(&RuleSource::Manifest).unwrap(),
            "\"manifest\""
        );
        assert_eq!(serde_json::to_string(&RuleSource::Taw).unwrap(), "\"taw\"");
    }
}
