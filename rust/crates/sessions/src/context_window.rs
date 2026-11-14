//! Shared context-window authority for admission and session usage reports.
use crate::{PROVIDER_MODELS_NAMESPACE, SessionFault, SessionRepository, SessionsError};
use serde_json::Value;

pub async fn resolve<R: SessionRepository>(
    store: &R,
    descriptor: &Value,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(u64, &'static str), SessionsError<SessionFault<R>>> {
    if let Some(window) = positive(&descriptor["context"]["windowTokens"]) {
        return Ok((window, "descriptor"));
    }
    if let (Some(provider), Some(model)) = (provider, model) {
        for namespace in [PROVIDER_MODELS_NAMESPACE, "http.provider-catalog"] {
            let catalog = store.get_setting(namespace, provider).await?;
            let window = catalog.as_ref().and_then(|catalog| {
                let row = catalog
                    .as_array()?
                    .iter()
                    .find(|row| row["modelId"].as_str() == Some(model))?;
                positive(&row["contextWindowTokens"])
            });
            if let Some(window) = window {
                return Ok((window, "model"));
            }
        }
    }
    Ok((crate::FALLBACK_CONTEXT_WINDOW_TOKENS as u64, "fallback"))
}

fn positive(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
}

/// Reapply the turn's admitted context instead of live future settings.
pub fn with_snapshot(mut descriptor: Value, snapshot: Option<&Value>) -> Option<Value> {
    let Some(snapshot) = snapshot else {
        return Some(descriptor);
    };
    if snapshot.is_null() {
        descriptor.as_object_mut()?.remove("context");
    } else {
        descriptor["context"] = snapshot.clone();
    }
    crate::read_session_descriptor(&serde_json::json!({"agentkit.session": descriptor}))
}
