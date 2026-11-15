//! Token estimate calibration (roadmap 2.5, spec "Token estimate
//! calibration"). The estimate behind every context threshold is
//! `chars / 4`; a provider reports the prompt tokens it really counted, and
//! the ratio between the two is smoothed into a per session and model factor
//! `rho`: `rho_n = 0.7 rho_{n-1} + 0.3 (P_actual / P_estimated)`, starting at
//! 1 and clamped to [0.6, 2.0]. The calibrated estimate is `E = rho *
//! chars / 4`; [`ContextBudget::message_budget`] applies it by scaling the
//! budget the thresholds compare against, so masking and compaction see E
//! without every token count in the pipeline carrying the factor.
//!
//! `rho` lives in the chat metadata (one entry per model), so a restart or
//! another worker picks up where the session left off.
//!
//! [`ContextBudget::message_budget`]: super::context_budget::ContextBudget::message_budget

use super::session_meta;
use crate::ExecutionContext;
use serde_json::{Value, json};
use std::sync::Mutex;

/// Chat metadata key: `{ "<model>": { "rho": number, "samples": integer } }`.
pub(super) const CALIBRATION_METADATA_KEY: &str = "agentkit.session.tokenCalibration";
pub(super) const RHO_MIN: f64 = 0.6;
pub(super) const RHO_MAX: f64 = 2.0;
const KEEP: f64 = 0.7;

fn clamp(rho: f64) -> f64 {
    if rho.is_finite() {
        rho.clamp(RHO_MIN, RHO_MAX)
    } else {
        1.0
    }
}

/// The next factor after one request. A request with no estimate or no
/// reported prompt tokens carries no information and leaves `prior` as it is.
pub(super) fn next_rho(prior: f64, actual: u64, estimated: u64) -> f64 {
    if actual == 0 || estimated == 0 {
        return clamp(prior);
    }
    clamp(KEEP * prior + (1.0 - KEEP) * (actual as f64 / estimated as f64))
}

/// The session's factor for `model`; 1 until a request has been observed.
pub(super) fn read(metadata: &Value, model: &str) -> f64 {
    metadata[CALIBRATION_METADATA_KEY][model]["rho"]
        .as_f64()
        .map_or(1.0, clamp)
}

/// Prompt tokens the provider billed for one request: uncached input plus
/// cache reads and writes (UsageRecord). A usage event that carries none of
/// them (an older provider) falls back to its legacy `promptTokens`.
pub(super) fn prompt_tokens(usage: &Value) -> Option<u64> {
    let fields = [
        "input_uncached",
        "cache_read",
        "cache_write_short",
        "cache_write_long",
    ];
    let counted: Vec<u64> = fields
        .iter()
        .filter_map(|field| usage[*field].as_u64())
        .collect();
    if counted.is_empty() {
        return usage["promptTokens"].as_u64().filter(|n| *n > 0);
    }
    Some(counted.iter().sum::<u64>()).filter(|n| *n > 0)
}

/// One run's view of the factor: the request estimate noted at admission, and
/// the factor kept in step with every final usage report.
pub(crate) struct Tracker {
    context: ExecutionContext,
    chat_id: String,
    model: String,
    rho: Mutex<f64>,
    estimate: Mutex<Option<u64>>,
}

impl Tracker {
    pub(super) fn new(context: ExecutionContext, chat_id: &str, model: &str, rho: f64) -> Self {
        Self {
            context,
            chat_id: chat_id.to_owned(),
            model: model.to_owned(),
            rho: Mutex::new(clamp(rho)),
            estimate: Mutex::new(None),
        }
    }

    /// The raw (uncalibrated) estimate of the request about to be sent.
    pub(super) fn note_estimate(&self, tokens: u64) {
        *self.estimate.lock().unwrap_or_else(|e| e.into_inner()) = Some(tokens);
    }

    /// Folds one `run.usage` payload into the factor and stores it. Only the
    /// final report of a call counts, and only when an estimate was noted for
    /// it. Calibration is a hint: a store failure leaves the factor in memory
    /// and never fails the turn.
    pub(super) async fn observe(&self, usage: &Value) {
        if usage["finalForCall"] != true {
            return;
        }
        let Some(actual) = prompt_tokens(usage) else {
            return;
        };
        let Some(estimated) = self
            .estimate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        // The stored factor, not this run's copy, is what the new sample
        // moves: another worker may have stored one since this run began.
        let mut next = *self.rho.lock().unwrap_or_else(|e| e.into_inner());
        let model = self.model.clone();
        let stored = session_meta::update(&self.context, &self.chat_id, |metadata| {
            next = next_rho(read(metadata, &model), actual, estimated);
            let samples = metadata[CALIBRATION_METADATA_KEY][model.as_str()]["samples"]
                .as_u64()
                .unwrap_or(0);
            metadata[CALIBRATION_METADATA_KEY][model.as_str()] =
                json!({"rho": next, "samples": samples + 1});
        })
        .await;
        if let Err(error) = stored {
            let held = *self.rho.lock().unwrap_or_else(|e| e.into_inner());
            next = next_rho(held, actual, estimated);
            // A hint, never a failure of the turn: say it was not kept. A
            // lost warning has nowhere further to go.
            let warning = json!({"code": "calibration_not_stored",
                "message": format!("The token estimate calibration was not stored: {error}.")});
            self.context.emit("run.warning", warning).await.ok();
        }
        *self.rho.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn rho_starts_at_one_and_smooths_toward_the_observed_ratio() {
        assert!(close(next_rho(1.0, 2000, 1000), 1.3));
        assert!(close(next_rho(1.3, 2000, 1000), 0.7 * 1.3 + 0.6));
        assert!(close(next_rho(1.0, 500, 1000), 0.85));
    }

    #[test]
    fn rho_is_clamped_to_the_spec_range() {
        let mut rho = 1.0;
        for _ in 0..40 {
            rho = next_rho(rho, 100_000, 1000);
        }
        assert!(close(rho, RHO_MAX));
        for _ in 0..40 {
            rho = next_rho(rho, 1, 1000);
        }
        assert!(close(rho, RHO_MIN));
    }

    #[test]
    fn a_report_without_numbers_leaves_rho_alone() {
        assert!(close(next_rho(1.4, 0, 1000), 1.4));
        assert!(close(next_rho(1.4, 1000, 0), 1.4));
    }

    #[test]
    fn rho_is_read_per_model_and_defaults_to_one() {
        let metadata = json!({CALIBRATION_METADATA_KEY: {"a": {"rho": 1.5}, "b": {"rho": 9.0}}});
        assert!(close(read(&metadata, "a"), 1.5));
        assert!(close(read(&metadata, "b"), RHO_MAX));
        assert!(close(read(&metadata, "c"), 1.0));
        assert!(close(read(&json!({}), "a"), 1.0));
    }

    #[test]
    fn prompt_tokens_sum_uncached_reads_and_writes() {
        let usage = json!({"input_uncached": 100, "cache_read": 400, "cache_write_short": 20, "cache_write_long": 5});
        assert_eq!(prompt_tokens(&usage), Some(525));
        assert_eq!(prompt_tokens(&json!({"promptTokens": 77})), Some(77));
        assert_eq!(prompt_tokens(&json!({"output": 5})), None);
    }
}
