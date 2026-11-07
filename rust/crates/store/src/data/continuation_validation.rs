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

pub(crate) fn scope(value: &Value) -> Result<()> {
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
    if agentkit_contracts::stringify_javascript_value(value)?.len() > 1024 * 1024 {
        return Err(invalid());
    }
    let mut canonical = value.clone();
    canonical
        .as_object_mut()
        .ok_or_else(invalid)?
        .remove("runContent");
    if let Some(binding) = value.get("runContent") {
        fields(binding, &["runId", "digest"])?;
        let digest = binding["digest"].as_str().ok_or_else(invalid)?;
        if binding["runId"].as_str().is_none_or(str::is_empty)
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid());
        }
    }
    fields(
        &canonical,
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

pub(crate) fn stored(db: &Connection, record: &ProviderContinuationRecord) -> Result<()> {
    state(&record.state)?;
    if record
        .state
        .get("runContent")
        .is_some_and(|binding| binding["runId"].as_str() != Some(record.run_id.as_str()))
    {
        return Err(invalid());
    }
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

#[cfg(test)]
mod envelope_tests {
    use super::*;
    #[test]
    fn combined_content_binding_and_checkpoint_obey_one_mebibyte_limit() {
        let value = serde_json::json!({"version":1,"scope":{"providerId":"p","protocol":"responses","connectionId":"c","generation":1,"chatId":"chat","branchId":"branch","model":"model"},"messageCount":0,"messageDigest":"0".repeat(64),"inputItems":[{"type":"message","content":"x".repeat(512*1024)}],"runContent":{"runId":"r".repeat(512*1024),"digest":"0".repeat(64)}});
        assert!(
            agentkit_contracts::stringify_javascript_value(&value)
                .unwrap()
                .len()
                > 1024 * 1024
        );
        assert!(state(&value).is_err());
    }
}
