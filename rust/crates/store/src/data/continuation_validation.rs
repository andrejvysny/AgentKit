use super::*;

fn invalid() -> StoreError {
    StoreError::InvalidInput("invalid_provider_continuation".into())
}

fn fields(value: &Value, expected: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or_else(invalid)?;
    if object.len() != expected.len()
        || expected.iter().any(|key| !object.contains_key(*key))
        || !finite_numbers(value)
        || agentkit_contracts::stringify_javascript_value(value)?.len() > 1024 * 1024
    {
        return Err(invalid());
    }
    Ok(())
}

fn finite_numbers(value: &Value) -> bool {
    match value {
        Value::Number(_) => {
            agentkit_contracts::javascript_number(value).is_some_and(f64::is_finite)
        }
        Value::Array(items) => items.iter().all(finite_numbers),
        Value::Object(object) => object.values().all(finite_numbers),
        _ => true,
    }
}

pub(super) fn scope(value: &Value) -> Result<()> {
    fields(
        value,
        &[
            "providerId",
            "protocol",
            "connectionId",
            "generation",
            "chatId",
            "branchId",
            "model",
        ],
    )?;
    for key in ["providerId", "connectionId", "chatId", "branchId", "model"] {
        if value[key].as_str().is_none_or(str::is_empty) {
            return Err(invalid());
        }
    }
    if value["protocol"] != "responses" || crate::util::safe_integer(&value["generation"]).is_none()
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn state(value: &Value) -> Result<()> {
    fields(
        value,
        &[
            "version",
            "scope",
            "messageCount",
            "messageDigest",
            "inputItems",
        ],
    )?;
    scope(&value["scope"])?;
    if crate::util::safe_integer(&value["version"]) != Some(1)
        || crate::util::safe_integer(&value["messageCount"]).is_none()
    {
        return Err(invalid());
    }
    let digest = value["messageDigest"].as_str().ok_or_else(invalid)?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid());
    }
    let items = value["inputItems"].as_array().ok_or_else(invalid)?;
    if items.len() > 1024 || items.iter().any(|item| !item.is_object()) {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn scopes_equal(left: &Value, right: &Value) -> bool {
    [
        "providerId",
        "protocol",
        "connectionId",
        "chatId",
        "branchId",
        "model",
    ]
    .iter()
    .all(|key| left[*key] == right[*key])
        && crate::util::safe_integer(&left["generation"])
            == crate::util::safe_integer(&right["generation"])
}

pub(super) fn stored(db: &Connection, record: &ProviderContinuationRecord) -> Result<()> {
    state(&record.state)?;
    let binding: Option<(String, String)> = db
        .query_row(
            "SELECT chat_id,scope FROM run_bindings WHERE run_id=?",
            [&record.run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (chat, scope) = binding
        .ok_or_else(|| StoreError::InvalidInput("provider_continuation_scope_mismatch".into()))?;
    let anchor: MessageRecord = required(db, "messages", "id", &record.anchor_message_id)?;
    if anchor.chat_id != chat
        || anchor.run_id.as_deref() != Some(&record.run_id)
        || !scopes_equal(
            &record.state["scope"],
            &agentkit_contracts::parse_javascript_json(&scope)?,
        )
    {
        return Err(StoreError::InvalidInput(
            "provider_continuation_scope_mismatch".into(),
        ));
    }
    Ok(())
}
