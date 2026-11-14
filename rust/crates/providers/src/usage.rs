use agentkit_core::{CoreError, UsageRecord};
use serde_json::Value;

const MAX_SAFE: u64 = 9_007_199_254_740_991;

pub(crate) fn count(value: &Value) -> Result<u64, CoreError> {
    let number = value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= MAX_SAFE as f64 && n.fract() == 0.0);
    number.map(|n| n as u64).ok_or_else(malformed)
}

fn malformed() -> CoreError {
    CoreError::responses("malformed_usage")
}

fn detail(value: &Value) -> Result<Option<u64>, CoreError> {
    if value.is_number() {
        count(value).map(Some)
    } else {
        Ok(None)
    }
}

fn difference(total: Option<u64>, subset: Option<u64>) -> Result<Option<u64>, CoreError> {
    match (total, subset) {
        (Some(total), Some(subset)) => total.checked_sub(subset).map(Some).ok_or_else(malformed),
        _ => Ok(None),
    }
}

#[derive(Default)]
struct Writes {
    total: Option<u64>,
    short: Option<u64>,
    long: Option<u64>,
}

fn writes(details: &Value, responses: bool) -> Result<Writes, CoreError> {
    let aggregate = detail(&details["cache_write_tokens"])?;
    let split = if responses {
        &details["cache_write_tokens_details"]
    } else {
        &Value::Null
    };
    let mut short = detail(&split["ttl_30m_tokens"])?;
    let mut long = detail(&split["ttl_12h_tokens"])?;
    let sum = match (short, long) {
        (Some(a), Some(b)) => Some(
            a.checked_add(b)
                .filter(|v| *v <= MAX_SAFE)
                .ok_or_else(malformed)?,
        ),
        _ => None,
    };
    if let Some(total) = aggregate {
        if sum.is_some_and(|n| n != total)
            || short.is_some_and(|n| n > total)
            || long.is_some_and(|n| n > total)
        {
            return Err(malformed());
        }
        if total == 0 {
            short = Some(0);
            long = Some(0);
        }
    }
    Ok(Writes {
        total: aggregate.or(sum),
        short,
        long,
    })
}

pub(crate) fn normalized_usage(
    data: &Value,
    raw: &Value,
    responses: bool,
) -> Result<UsageRecord, CoreError> {
    let cache_read = detail(&data["cachedPromptTokens"])?;
    let reasoning = detail(&data["reasoningTokens"])?;
    let input = detail(&data["promptTokens"])?;
    let writes = writes(&raw["prompt_tokens_details"], responses)?;
    if let Some(input) = input {
        let lower_writes = match writes.total {
            Some(total) => total,
            None => writes
                .short
                .unwrap_or(0)
                .checked_add(writes.long.unwrap_or(0))
                .ok_or_else(malformed)?,
        };
        let lower = cache_read
            .unwrap_or(0)
            .checked_add(lower_writes)
            .filter(|n| *n <= MAX_SAFE)
            .ok_or_else(malformed)?;
        if lower > input {
            return Err(malformed());
        }
    }
    let remaining = difference(input, cache_read)?;
    difference(input, writes.total)?;
    Ok(UsageRecord {
        input_uncached: difference(remaining, writes.total)?,
        cache_read,
        cache_write_short: writes.short,
        cache_write_long: writes.long,
        output: difference(detail(&data["completionTokens"])?, reasoning)?,
        reasoning,
        provider_raw: raw.clone(),
    })
}
