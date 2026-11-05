//! Operator surface over uncertain execution receipts: list them and resolve
//! one explicitly. Policy (ownership, active-run refusal, idempotency) lives
//! in the host session service; this file only parses and serializes.
use super::{SessionService, optional_string, parse_object};
use crate::errors;
use agentkit_store::{ExecutionReceiptRecord, ResolutionOutcome};
use napi::bindgen_prelude::*;
use serde_json::Value;

const RESOLVE_KEYS: [&str; 6] = [
    "sessionId",
    "executionId",
    "outcome",
    "result",
    "note",
    "resolvedBy",
];

const MAX_RESULT_BYTES: usize = agentkit_store::MAX_RESOLUTION_RESULT_BYTES;

pub(super) struct ResolveInput {
    pub session_id: String,
    pub execution_id: String,
    pub outcome: ResolutionOutcome,
    pub result: Option<Value>,
    pub note: Option<String>,
    pub resolved_by: Option<String>,
}

fn required_string(object: &Value, key: &str) -> Result<String> {
    optional_string(object, key)?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid(format!("{key} is required")))
}

pub(super) fn parse_resolve(text: &str) -> Result<ResolveInput> {
    let object = parse_object(text, "the resolve input")?;
    if let Some(unknown) = object
        .as_object()
        .and_then(|map| map.keys().find(|key| !RESOLVE_KEYS.contains(&key.as_str())))
    {
        return Err(errors::invalid(format!("unknown field \"{unknown}\"")));
    }
    let outcome = match required_string(&object, "outcome")?.as_str() {
        "completed" => ResolutionOutcome::Completed,
        "not_started" => ResolutionOutcome::NotStarted,
        "acknowledged" => ResolutionOutcome::Acknowledged,
        other => {
            return Err(errors::invalid(format!(
                "unknown outcome \"{other}\" (expected completed, not_started or acknowledged)"
            )));
        }
    };
    let result = object
        .get("result")
        .filter(|value| !value.is_null())
        .cloned();
    match (outcome, &result) {
        (ResolutionOutcome::Completed, None) => {
            return Err(errors::invalid(
                "result is required for a completed outcome",
            ));
        }
        (ResolutionOutcome::Completed, Some(_)) | (_, None) => {}
        (_, Some(_)) => {
            return Err(errors::invalid(
                "result is only allowed for a completed outcome",
            ));
        }
    }
    if let Some(result) = &result
        && result.to_string().len() > MAX_RESULT_BYTES
    {
        return Err(errors::invalid("result exceeds 262144 bytes"));
    }
    let resolved_by = optional_string(&object, "resolvedBy")?;
    if resolved_by
        .as_ref()
        .is_some_and(|actor| actor.chars().count() > 200)
    {
        return Err(errors::invalid("resolvedBy exceeds 200 characters"));
    }
    Ok(ResolveInput {
        session_id: required_string(&object, "sessionId")?,
        execution_id: required_string(&object, "executionId")?,
        outcome,
        result,
        note: optional_string(&object, "note")?,
        resolved_by,
    })
}

fn records_json(records: &[ExecutionReceiptRecord]) -> Result<String> {
    serde_json::to_string(records).map_err(|_| errors::internal("serializing execution receipts"))
}

#[napi]
impl SessionService {
    #[napi]
    pub async fn list_uncertain_executions(&self, session_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let records = core
            .service
            .list_uncertain_executions(&session_id)
            .await
            .map_err(errors::session)?;
        records_json(&records)
    }

    #[napi]
    pub async fn resolve_uncertain_execution(&self, input_json: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_resolve(&input_json)?;
        let record = core
            .service
            .resolve_uncertain_execution(
                &input.session_id,
                &input.execution_id,
                input.outcome,
                input.result,
                input.note,
                input.resolved_by,
            )
            .await
            .map_err(errors::session)?;
        serde_json::to_string(&record)
            .map_err(|_| errors::internal("serializing execution receipt"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn code_of(error: Error) -> String {
        let payload: Value = serde_json::from_str(&error.reason).unwrap();
        payload["code"].as_str().unwrap().to_owned()
    }

    fn parse(body: Value) -> Result<ResolveInput> {
        parse_resolve(&body.to_string())
    }

    #[test]
    fn accepts_each_outcome_with_its_result_rules() {
        let base = json!({"sessionId": "s", "executionId": "e", "note": "n", "resolvedBy": "op"});
        let with = |extra: Value| {
            let mut body = base.clone();
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body
        };
        let done = parse(with(json!({"outcome": "completed", "result": "ok"}))).unwrap();
        assert_eq!(done.outcome, ResolutionOutcome::Completed);
        assert_eq!(done.result, Some(json!("ok")));
        assert_eq!(done.resolved_by.as_deref(), Some("op"));
        let object_result = parse(with(json!({"outcome": "completed", "result": {"a": 1}})));
        assert_eq!(object_result.unwrap().result, Some(json!({"a": 1})));
        for outcome in ["not_started", "acknowledged"] {
            let parsed = parse(with(json!({"outcome": outcome}))).unwrap();
            assert!(parsed.result.is_none());
        }
    }

    #[test]
    fn refuses_malformed_input_with_invalid_input() {
        let valid = || json!({"sessionId": "s", "executionId": "e", "outcome": "not_started"});
        let mut cases: Vec<Value> = vec![];
        for key in ["sessionId", "executionId", "outcome"] {
            let mut body = valid();
            body.as_object_mut().unwrap().remove(key);
            cases.push(body);
        }
        for (key, value) in [
            ("outcome", json!("done")),
            ("outcome", json!(1)),
            ("sessionId", json!("")),
            ("note", json!(5)),
            ("resolvedBy", json!([])),
            ("surprise", json!(true)),
            ("result", json!("x")),
            ("resolvedBy", json!("x".repeat(201))),
        ] {
            let mut body = valid();
            body[key] = value;
            cases.push(body);
        }
        let mut missing_result = valid();
        missing_result["outcome"] = json!("completed");
        cases.push(missing_result);
        let mut huge = valid();
        huge["outcome"] = json!("completed");
        huge["result"] = json!("y".repeat(262_145));
        cases.push(huge);
        for body in cases {
            let error = parse(body.clone())
                .err()
                .unwrap_or_else(|| panic!("{body}"));
            assert_eq!(code_of(error), "invalid_input", "{body}");
        }
        assert_eq!(
            code_of(parse_resolve("[1]").err().unwrap()),
            "invalid_input"
        );
        assert_eq!(code_of(parse_resolve("{").err().unwrap()), "invalid_input");
    }
}
