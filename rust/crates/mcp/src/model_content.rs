use serde_json::{Value, json};

/// Host attestation of the selected model's supported attachment types.
#[derive(Clone, Copy, Debug, Default)]
pub struct ModelContentCapabilities {
    pub image: bool,
    pub audio: bool,
}

#[derive(Debug, Default)]
pub struct ModelContentProjection {
    pub text: String,
    pub sources: Vec<Value>,
    pub attachments: Vec<Value>,
    pub unsupported: Vec<String>,
}

/// Remote resources are descriptive, untrusted sources; projection never fetches them.
pub fn project_model_content(
    canonical_id: &str,
    alias: &str,
    tool: &str,
    content: &[Value],
    capabilities: ModelContentCapabilities,
) -> ModelContentProjection {
    let mut projection = ModelContentProjection::default();
    let mut text = Vec::new();
    for part in content {
        match part["type"].as_str() {
            Some("text") => {
                if let Some(value) = part["text"].as_str() {
                    text.push(value.to_owned());
                }
            }
            Some("resource") => {
                let resource = &part["resource"];
                if let Some(uri) = resource["uri"].as_str() {
                    projection
                        .sources
                        .push(source(canonical_id, alias, tool, uri, uri));
                    if let Some(value) = resource["text"].as_str() {
                        text.push(format!("[Source: {uri}]\n{value}"));
                    }
                }
            }
            Some("resource_link") if part["uri"].is_string() => {
                let uri = part["uri"].as_str().unwrap();
                let label = part["name"].as_str().unwrap_or(uri);
                projection
                    .sources
                    .push(source(canonical_id, alias, tool, uri, label));
                text.push(format!(
                    "[Resource: {uri}] {}",
                    part["description"].as_str().unwrap_or("")
                ));
            }
            Some(kind) => match attachment(part, capabilities) {
                Some(value) => projection.attachments.push(value),
                None if !projection.unsupported.iter().any(|value| value == kind) => {
                    projection.unsupported.push(kind.into())
                }
                None => {}
            },
            None => {}
        }
    }
    projection.text = text.join("\n");
    projection
}

fn source(canonical_id: &str, alias: &str, tool: &str, uri: &str, label: &str) -> Value {
    json!({"id":format!("{canonical_id}:{uri}"),"kind":"mcp-resource","label":label,
        "path":uri,"metadata":{"serverAlias":alias,"tool":tool,"trusted":false}})
}

fn attachment(part: &Value, capabilities: ModelContentCapabilities) -> Option<Value> {
    let data = part["data"].as_str()?;
    let media = part["mimeType"].as_str()?;
    match part["type"].as_str()? {
        "image" if capabilities.image && media.starts_with("image/") && media_type(media) => {
            Some(json!({"type":"image","source":{"kind":"data","mediaType":media,"base64":data}}))
        }
        "audio"
            if capabilities.audio && matches!(media, "audio/wav" | "audio/mpeg" | "audio/mp3") =>
        {
            Some(
                json!({"type":"audio","data":data,"format":if media=="audio/wav" {"wav"} else {"mp3"}}),
            )
        }
        _ => None,
    }
}

fn media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    [kind, subtype].iter().all(|part| {
        part.as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
    })
}
