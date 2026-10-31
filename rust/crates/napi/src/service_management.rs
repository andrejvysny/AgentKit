//! Session management methods exposed without duplicating host policy.
use super::{SessionService, parse_object, session_state_json};
use crate::errors;
use napi::bindgen_prelude::*;
use serde_json::Value;

#[napi]
impl SessionService {
    #[napi]
    pub async fn set_execution_mode(&self, session_id: String, mode: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(core
            .service
            .set_execution_mode(&session_id, &mode)
            .await
            .map_err(errors::session)?
            .to_string())
    }

    #[napi]
    pub async fn list_approval_grants(&self, session_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(Value::Array(
            core.service
                .list_approval_grants(&session_id)
                .await
                .map_err(errors::session)?,
        )
        .to_string())
    }

    #[napi]
    pub async fn revoke_approval_grant(
        &self,
        session_id: String,
        grant_id: String,
    ) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(core
            .service
            .revoke_approval_grant(&session_id, &grant_id)
            .await
            .map_err(errors::session)?
            .to_string())
    }

    #[napi]
    pub async fn compact(&self, session_id: String) -> Result<()> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.service
            .compact(&session_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn fork(&self, session_id: String, from_message_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let state = core
            .service
            .fork(&session_id, &from_message_id)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn set_model(&self, session_id: String, input_json: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_object(&input_json, "model input")?;
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| errors::invalid("model must be a string"))?;
        let provider = optional_string(&input, "providerId")?;
        let effort = match input.get("reasoningEffort") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(value)) => Some(Some(value.as_str())),
            _ => return Err(errors::invalid("reasoningEffort must be a string or null")),
        };
        let state = core
            .service
            .set_model(&session_id, provider, model, effort)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn cancel_queued_input(&self, session_id: String, input_id: String) -> Result<bool> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.service
            .cancel_queued_input(&session_id, &input_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn clear_queue(&self, session_id: String) -> Result<u32> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let count = core
            .service
            .clear_queue(&session_id)
            .await
            .map_err(errors::session)?;
        u32::try_from(count).map_err(|_| errors::internal("queue count overflow"))
    }

    #[napi]
    pub async fn configure(&self, session_id: String, input_json: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_object(&input_json, "configuration")?;
        let skills = match input.get("skills") {
            None => None,
            Some(Value::Array(values)) => Some(values.clone()),
            _ => return Err(errors::invalid("skills must be an array")),
        };
        let trusted = match input.get("workspaceTrusted") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            _ => return Err(errors::invalid("workspaceTrusted must be a boolean")),
        };
        let state = core
            .service
            .configure(&session_id, skills, input.get("context").cloned(), trusted)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }
}

fn optional_string<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(errors::invalid(format!("{key} must be a string"))),
    }
}
