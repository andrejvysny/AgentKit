//! The versioned qualification record (R21, TU-36). A model is qualified only
//! for the exact tuple the run measured; any change to the model, provider,
//! profile, tool schemas, prompts or toolsets means the record no longer
//! applies and a new run is needed.
use super::matrix::Thresholds;
use super::scenario::Verdict;
use agentkit_kernel::canonical_sha256;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub const QUALIFICATION_RECORD_VERSION: &str = "agentkit.tool-use-qualification/1";

/// Digest of canonical JSON, for tool definitions and prompts.
pub fn digest_json(value: &Value) -> String {
    canonical_sha256(value)
}

/// What a qualification is valid for. Equality is the validity test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QualificationKey {
    pub model: String,
    pub provider: String,
    /// Server or proxy behind the provider (for example `litellm`).
    pub server: String,
    pub profile_id: String,
    pub profile_version: String,
    pub profile_digest: String,
    pub schema_digest: String,
    pub prompt_digest: String,
    /// Sorted.
    pub toolsets: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub toolset: String,
    pub lang: String,
    pub passed: bool,
    pub calls: u32,
    pub invalid_calls: u32,
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolsetResult {
    pub toolset: String,
    pub scenarios: u32,
    pub passed: u32,
    pub success_rate: f64,
    pub invalid_rate: f64,
    pub qualified: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QualificationRecord {
    pub record_version: String,
    #[serde(flatten)]
    pub key: QualificationKey,
    pub thresholds: Thresholds,
    pub scenarios: Vec<ScenarioResult>,
    pub toolset_results: Vec<ToolsetResult>,
    /// Toolsets that meet the thresholds; feeds `qualifiedToolsets`.
    pub qualified_toolsets: Vec<String>,
    /// RFC 3339 UTC.
    pub timestamp: String,
    pub agentkit_commit: String,
}

fn rate(part: u32, whole: u32) -> f64 {
    if whole == 0 {
        0.0
    } else {
        f64::from(part) / f64::from(whole)
    }
}

impl ScenarioResult {
    pub fn new(scenario: &str, toolset: &str, lang: &str, verdict: Verdict) -> Self {
        Self {
            scenario: scenario.to_owned(),
            toolset: toolset.to_owned(),
            lang: lang.to_owned(),
            passed: verdict.passed,
            calls: verdict.calls,
            invalid_calls: verdict.invalid_calls,
            failures: verdict.failures,
        }
    }
}

impl QualificationRecord {
    pub fn build(
        mut key: QualificationKey,
        thresholds: Thresholds,
        scenarios: Vec<ScenarioResult>,
        timestamp: String,
        agentkit_commit: String,
    ) -> Self {
        key.toolsets.sort();
        key.toolsets.dedup();
        let mut by_set: BTreeMap<&str, Vec<&ScenarioResult>> = BTreeMap::new();
        for result in &scenarios {
            by_set.entry(&result.toolset).or_default().push(result);
        }
        let toolset_results: Vec<ToolsetResult> = by_set
            .into_iter()
            .map(|(toolset, set)| {
                let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                let total = count(set.len());
                let passed = count(set.iter().filter(|r| r.passed).count());
                let calls: u32 = set.iter().map(|r| r.calls).sum();
                let invalid: u32 = set.iter().map(|r| r.invalid_calls).sum();
                let (success_rate, invalid_rate) = (rate(passed, total), rate(invalid, calls));
                ToolsetResult {
                    toolset: toolset.to_owned(),
                    scenarios: total,
                    passed,
                    success_rate,
                    invalid_rate,
                    qualified: thresholds.qualifies(success_rate, invalid_rate),
                }
            })
            .collect();
        let qualified_toolsets = toolset_results
            .iter()
            .filter(|t| t.qualified)
            .map(|t| t.toolset.clone())
            .collect();
        Self {
            record_version: QUALIFICATION_RECORD_VERSION.to_owned(),
            key,
            thresholds,
            scenarios,
            toolset_results,
            qualified_toolsets,
            timestamp,
            agentkit_commit,
        }
    }

    /// Whether this record qualifies `toolset` for exactly the tuple `current`.
    pub fn qualifies(&self, current: &QualificationKey, toolset: &str) -> bool {
        let mut current = current.clone();
        current.toolsets.sort();
        self.key == current && self.qualified_toolsets.iter().any(|t| t == toolset)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}
