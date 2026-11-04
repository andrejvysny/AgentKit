use serde::{Deserialize, Serialize};

/// Advisory review + watchdog configuration. Independently configurable
/// model per surface (advisor vs watchdog) so a host can spend differently
/// on demand versus background vigilance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AdvisorConfig {
    pub enabled: bool,
    /// Model for explicit `advisor_review` calls. `None` uses the bound
    /// provider's default.
    pub model: Option<String>,
    /// Model for automatic watchdog reviews — settable separately so
    /// background vigilance can cost less (or more) than explicit asks.
    pub watchdog_model: Option<String>,
    /// Tool-loop bound for one review.
    pub max_iterations: u32,
    pub max_tool_calls_per_iteration: usize,
    pub tool_timeout_ms: u64,
    /// Per-review execution budget.
    pub provider_requests: Option<u64>,
    pub overall_timeout_ms: Option<u64>,
    /// Hard cap on findings kept from one report (model output is bounded;
    /// the surplus is summarized away, never silently kept in the DB).
    pub max_findings: usize,
    // -- loop & spend guards ----------------------------------------------
    /// Concurrent reviews process-wide.
    pub max_review_concurrency: usize,
    /// Per-chat+trigger cooldown between automatic reviews.
    pub cooldown_ms: i64,
    /// Rolling-window watchdog review budget.
    pub reviews_per_window: u32,
    pub window_ms: i64,
    // -- trigger thresholds -------------------------------------------------
    /// Consecutive failed tool results in one run that raise a review.
    pub trigger_consecutive_tool_failures: u32,
    /// Repetitions of the SAME error code that raise a review.
    pub trigger_repeated_error_code: u32,
    /// Distinct files one turn may touch before a scope-deviation review.
    pub trigger_edit_count: u32,
    /// Consecutive failed turns of a session that raise a review.
    pub trigger_turn_failures: u32,
    /// When true, a critical finding is also delivered to the reviewed
    /// session as a durable steer input.
    pub escalate_critical: bool,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            watchdog_model: None,
            max_iterations: 6,
            max_tool_calls_per_iteration: 4,
            tool_timeout_ms: 15_000,
            provider_requests: Some(12),
            overall_timeout_ms: Some(120_000),
            max_findings: 12,
            max_review_concurrency: 2,
            cooldown_ms: 180_000,
            reviews_per_window: 6,
            window_ms: 600_000,
            trigger_consecutive_tool_failures: 3,
            trigger_repeated_error_code: 3,
            trigger_edit_count: 12,
            trigger_turn_failures: 2,
            escalate_critical: true,
        }
    }
}

impl AdvisorConfig {
    /// Clamp to sane floors; a nonsense configuration degrades to defaults
    /// rather than disabling the safety floors.
    pub fn effective(mut self) -> Self {
        let defaults = AdvisorConfig::default();
        if self.max_iterations < 1 {
            self.max_iterations = defaults.max_iterations;
        }
        if self.max_tool_calls_per_iteration < 1 {
            self.max_tool_calls_per_iteration = defaults.max_tool_calls_per_iteration;
        }
        if self.max_findings < 1 {
            self.max_findings = defaults.max_findings;
        }
        if self.max_review_concurrency < 1 {
            self.max_review_concurrency = defaults.max_review_concurrency;
        }
        if self.cooldown_ms < 1_000 {
            self.cooldown_ms = defaults.cooldown_ms;
        }
        if self.reviews_per_window < 1 {
            self.reviews_per_window = defaults.reviews_per_window;
        }
        if self.window_ms < 10_000 {
            self.window_ms = defaults.window_ms;
        }
        if self.trigger_consecutive_tool_failures < 2 {
            self.trigger_consecutive_tool_failures = defaults.trigger_consecutive_tool_failures;
        }
        if self.trigger_repeated_error_code < 2 {
            self.trigger_repeated_error_code = defaults.trigger_repeated_error_code;
        }
        if self.trigger_edit_count < 2 {
            self.trigger_edit_count = defaults.trigger_edit_count;
        }
        if self.trigger_turn_failures < 2 {
            self.trigger_turn_failures = defaults.trigger_turn_failures;
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
}

impl FindingSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// Design/implementation risk in the reviewed work.
    ImplementationRisk,
    /// Repeated failures of the same kind.
    RepeatedFailure,
    /// Work drifting outside the requested scope.
    ScopeDeviation,
    /// Change that may have regressed existing behavior.
    Regression,
    /// Advisor output could not be validated.
    OutputInvalid,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImplementationRisk => "implementation_risk",
            Self::RepeatedFailure => "repeated_failure",
            Self::ScopeDeviation => "scope_deviation",
            Self::Regression => "regression",
            Self::OutputInvalid => "output_invalid",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "implementation_risk" => Some(Self::ImplementationRisk),
            "repeated_failure" => Some(Self::RepeatedFailure),
            "scope_deviation" => Some(Self::ScopeDeviation),
            "regression" => Some(Self::Regression),
            "output_invalid" => Some(Self::OutputInvalid),
            _ => None,
        }
    }
}

/// One review request. `chat_id` is the SESSION UNDER REVIEW (a review has
/// exactly one subject session); `focus` tells the advisor what to look at.
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    pub chat_id: String,
    pub focus: String,
    /// Durable evidence the caller already holds (recent failures, edited
    /// files), handed to the advisor as the starting point of its own
    /// investigation.
    pub evidence_hint: Option<serde_json::Value>,
}
