use agentkit_core::UsageRecord;
use agentkit_native::NativeUsage;
use serde_json::{Value, json};

fn has_usage_counters(usage: &NativeUsage) -> bool {
    let normalized = usage.normalized.as_ref().is_some_and(|n| {
        [
            n.input_uncached,
            n.cache_read,
            n.cache_write_short,
            n.cache_write_long,
            n.output,
            n.reasoning,
        ]
        .iter()
        .any(Option::is_some)
    });
    let reported = usage.per_turn.as_ref().is_some_and(|n| {
        [
            n.input_tokens,
            n.output_tokens,
            n.cache_read_tokens,
            n.cache_write_tokens,
        ]
        .iter()
        .any(Option::is_some)
    });
    normalized || reported
}

pub(crate) fn remember(pending: &mut Option<NativeUsage>, usage: &NativeUsage) {
    if !has_usage_counters(usage) {
        return;
    }
    let Some(current) = pending else {
        *pending = Some(usage.clone());
        return;
    };
    if let Some(update) = &usage.normalized {
        let counts = current.normalized.get_or_insert_with(Default::default);
        counts.input_uncached = update.input_uncached.or(counts.input_uncached);
        counts.cache_read = update.cache_read.or(counts.cache_read);
        counts.cache_write_short = update.cache_write_short.or(counts.cache_write_short);
        counts.cache_write_long = update.cache_write_long.or(counts.cache_write_long);
        counts.output = update.output.or(counts.output);
        counts.reasoning = update.reasoning.or(counts.reasoning);
    }
    if let Some(update) = &usage.per_turn {
        let counts = current.per_turn.get_or_insert_with(Default::default);
        counts.input_tokens = update.input_tokens.or(counts.input_tokens);
        counts.output_tokens = update.output_tokens.or(counts.output_tokens);
        counts.cache_read_tokens = update.cache_read_tokens.or(counts.cache_read_tokens);
        counts.cache_write_tokens = update.cache_write_tokens.or(counts.cache_write_tokens);
    }
    if usage.provider_raw.is_some() {
        current.provider_raw = usage.provider_raw.clone();
    }
}

fn add(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    left.zip(right)
        .and_then(|(left, right)| left.checked_add(right))
        .filter(|count| *count <= 9_007_199_254_740_991)
}

fn legacy_counts(usage: &NativeUsage, record: &UsageRecord, data: &mut Value) {
    let tokens = usage.per_turn.as_ref();
    let writes = add(record.cache_write_short, record.cache_write_long)
        .or_else(|| tokens.and_then(|t| t.cache_write_tokens));
    let prompt = add(add(record.input_uncached, record.cache_read), writes);
    let completion =
        add(record.output, record.reasoning).or_else(|| tokens.and_then(|t| t.output_tokens));
    for (field, count) in [
        ("promptTokens", prompt),
        ("completionTokens", completion),
        ("totalTokens", add(prompt, completion)),
        ("cachedPromptTokens", record.cache_read),
        ("reasoningTokens", record.reasoning),
    ] {
        if let Some(count) = count.filter(|n| *n <= 9_007_199_254_740_991) {
            data[field] = count.into();
        }
    }
}

/// Project adapter-normalized counters without interpreting vendor wire data.
pub(crate) fn event_data(usage: &NativeUsage, call_id: &str, attempt: u32, model: &str) -> Value {
    let normalized = usage.normalized.as_ref();
    let safe = |n: Option<u64>| n.filter(|n| *n <= 9_007_199_254_740_991);
    let record = UsageRecord {
        input_uncached: safe(normalized.and_then(|n| n.input_uncached)),
        cache_read: safe(normalized.and_then(|n| n.cache_read)),
        cache_write_short: safe(normalized.and_then(|n| n.cache_write_short)),
        cache_write_long: safe(normalized.and_then(|n| n.cache_write_long)),
        output: safe(normalized.and_then(|n| n.output)),
        reasoning: safe(normalized.and_then(|n| n.reasoning)),
        provider_raw: usage.provider_raw.clone().unwrap_or(Value::Null),
    };
    let mut data = json!({"callId": call_id, "attempt": attempt, "step": 0, "model": model, "source": "response", "finalForCall": true});
    legacy_counts(usage, &record, &mut data);
    if let Value::Object(fields) = serde_json::to_value(record).expect("usage record serializes") {
        data.as_object_mut()
            .expect("usage data is an object")
            .extend(fields);
    }
    if usage.provider_raw.is_none() {
        data.as_object_mut()
            .expect("usage data is an object")
            .remove("provider_raw");
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_native::NativeNormalizedUsage;

    #[test]
    fn partial_updates_keep_prior_counters_and_raw_evidence() {
        let mut pending = None;
        remember(
            &mut pending,
            &NativeUsage {
                normalized: Some(NativeNormalizedUsage {
                    input_uncached: Some(4),
                    ..Default::default()
                }),
                provider_raw: Some(json!({"input_tokens": 4})),
                ..Default::default()
            },
        );
        remember(
            &mut pending,
            &NativeUsage {
                normalized: Some(NativeNormalizedUsage {
                    output: Some(117),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let pending = pending.unwrap();
        let normalized = pending.normalized.as_ref().unwrap();
        assert_eq!(normalized.input_uncached, Some(4));
        assert_eq!(normalized.output, Some(117));
        assert_eq!(pending.provider_raw, Some(json!({"input_tokens": 4})));
    }

    #[test]
    fn preserves_legacy_totals_for_session_reports() {
        let usage = NativeUsage {
            normalized: Some(NativeNormalizedUsage {
                input_uncached: Some(4),
                cache_read: Some(1734),
                cache_write_short: Some(0),
                cache_write_long: Some(813),
                output: Some(117),
                reasoning: Some(48),
            }),
            ..Default::default()
        };
        let data = event_data(&usage, "c", 1, "m");
        assert_eq!(data["promptTokens"], 2551);
        assert_eq!(data["completionTokens"], 165);
        assert_eq!(data["totalTokens"], 2716);
    }

    #[test]
    fn projects_normalized_usage_and_preserves_unknowns() {
        let usage = NativeUsage {
            normalized: Some(NativeNormalizedUsage {
                input_uncached: Some(4),
                output: Some(117),
                reasoning: Some(48),
                ..Default::default()
            }),
            provider_raw: Some(json!({"reported": true})),
            ..Default::default()
        };
        let data = event_data(&usage, "attempt-1", 2, "model");
        assert_eq!(data["callId"], "attempt-1");
        assert_eq!(data["attempt"], 2);
        assert_eq!(data["input_uncached"], 4);
        assert_eq!(data["output"], 117);
        assert_eq!(data["reasoning"], 48);
        assert_eq!(data["provider_raw"], json!({"reported": true}));
        assert!(data.get("cache_read").is_none());
        assert!(
            event_data(&NativeUsage::default(), "c", 1, "m")
                .get("provider_raw")
                .is_none()
        );
    }
}
