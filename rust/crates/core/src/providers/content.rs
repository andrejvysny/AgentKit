use crate::{ChatMessage, CoreError};
use serde_json::{json, Value};
pub(crate) fn text(message: &ChatMessage, strict: bool) -> Result<String, CoreError> {
    if let Some(text) = message.content.as_str() {
        return Ok(text.into());
    }
    let parts = message
        .content
        .as_array()
        .ok_or_else(|| CoreError::responses("unsupported_modality"))?;
    let mut output = Vec::new();
    for part in parts {
        if let Some(text) = part["text"].as_str().filter(|_| part["type"] == "text") {
            output.push(text);
        } else if strict {
            return Err(CoreError::responses("unsupported_modality"));
        }
    }
    Ok(output.join(if strict { "" } else { "\n" }))
}
fn image_url(part: &Value) -> Option<String> {
    let source = &part["source"];
    match source["kind"].as_str()? {
        "url" => source["url"]
            .as_str()
            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
            .map(str::to_owned),
        "data" => {
            let media = source["mediaType"].as_str()?;
            let mut pieces = media.split('/');
            if !pieces.all(|piece| {
                piece
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                    && piece
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
            }) || media.matches('/').count() != 1
            {
                return None;
            }
            Some(format!(
                "data:{media};base64,{}",
                source["base64"].as_str()?
            ))
        }
        _ => None,
    }
}
pub(crate) fn parts(
    message: &ChatMessage,
    responses: bool,
    vision: bool,
) -> Result<(Value, bool), CoreError> {
    if let Some(text) = message.content.as_str() {
        return Ok((
            if responses {
                json!([{"type":"input_text","text":text}])
            } else {
                text.into()
            },
            false,
        ));
    }
    let input = message
        .content
        .as_array()
        .ok_or_else(|| CoreError::responses("unsupported_modality"))?;
    if !["user", "assistant"].contains(&message.role.as_str()) {
        return Ok((
            text(message, responses)?.into(),
            input.iter().any(|part| part["type"] != "text"),
        ));
    }
    let mut output = vec![];
    let mut dropped = false;
    for part in input {
        if part["type"] == "text" {
            output.push(
                json!({"type":if responses {"input_text"} else {"text"},"text":part["text"]}),
            );
        } else if part["type"] == "image" && part["source"]["kind"] == "ref" && !responses {
            continue;
        } else if part["type"] == "image" && vision && (!responses || message.role == "user") {
            if let Some(url) = image_url(part) {
                output.push(if responses {json!({"type":"input_image","image_url":url,"detail":part.get("detail").cloned().unwrap_or(json!("auto"))})}
                    else {let mut image=json!({"url":url});if let Some(detail)=part.get("detail") {image["detail"]=detail.clone();}json!({"type":"image_url","image_url":image})});
            } else if responses {
                return Err(CoreError::responses("unsupported_modality"));
            } else {
                dropped = true;
            }
        } else if responses {
            return Err(CoreError::responses("unsupported_modality"));
        } else {
            dropped = true;
        }
    }
    if output.is_empty() {
        return Ok((if responses { json!([]) } else { json!("") }, dropped));
    }
    Ok((output.into(), dropped))
}
