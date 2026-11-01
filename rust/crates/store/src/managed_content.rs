//! Managed content: prompt templates, managed skills, managed connections,
//! and run-content records (the managed-integrations recipe's backend).
//!
//! The behavior mirrored here is the TypeScript reference implementation —
//! `PromptTemplateService` + `core/prompts/templates.ts` (template
//! validation/rendering, immutable revisions), `SkillService` +
//! `skills/skill-service.ts` (frontmatter subset, content-hash revisions,
//! activation), `ConnectionService.save` (compare-and-swap revisions,
//! immutable account identity, unique normalized aliases, the
//! `transport-http` server-request validator), and `createRunContentProvider`
//! (skill selection rules). As everywhere in this store, Rust owns
//! validation, ordering, and atomicity; the binding port and the HTTP lane
//! are thin arms over these methods.
//!
//! No secrets live in these tables. Connection configs keep their
//! credential-shaped `transport.env`/`transport.headers` values redacted on
//! every read path, and `secretRefs` entries are refused outright on save —
//! the binding's deny-all posture (`docs/rust/native-binding-milestone.md`
//! "Open gaps": no OAuth, deny-all secret resolution) never accepts a
//! reference it cannot resolve. Connection lifecycle (connect/reconnect/
//! test) is the documented MCP-OAuth reduction and lives OUTSIDE this
//! module: these tables store configuration only.

use crate::{Result, SqliteStore, StoreError, transaction::atomic};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Marker prefix the binding port maps to the stable `mcp_secret_missing`
/// code (the same marker-classification convention the assistant-store
/// ports use for engine-side refusals a generic mapping cannot name).
pub const SECRET_REF_MARKER: &str =
    "secretRefs are not supported by the native connection store (deny-all posture): ";

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn is_token_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Scan `content` for `{{ key }}` tokens with the TypeScript grammar
/// `/\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}/g`; `on_token` sees every key
/// occurrence in order with its byte range. Returns false when a `{{` does
/// not open a valid token (a leftover expression the validator refuses).
fn for_each_token(content: &str, on_token: &mut impl FnMut(&str, usize, usize)) -> bool {
    let bytes = content.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'{' && bytes[index + 1] == b'{' {
            let start = index;
            index += 2;
            while index < bytes.len() && is_token_whitespace(bytes[index]) {
                index += 1;
            }
            let key_start = index;
            if index < bytes.len() && (bytes[index].is_ascii_alphabetic() || bytes[index] == b'_') {
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || bytes[index] == b'_'
                        || bytes[index] == b'-')
                {
                    index += 1;
                }
            }
            let key_end = index;
            while index < bytes.len() && is_token_whitespace(bytes[index]) {
                index += 1;
            }
            if key_end > key_start
                && index + 1 < bytes.len()
                && bytes[index] == b'}'
                && bytes[index + 1] == b'}'
            {
                let key = &content[key_start..key_end];
                let end = index + 2;
                on_token(key, start, end);
                index = end;
                continue;
            }
            return false;
        }
        index += 1;
    }
    true
}

fn has_leftover_expression(content: &str) -> bool {
    // Same probe as TypeScript: strip every valid token, then look for `{{`.
    let mut stripped = String::with_capacity(content.len());
    let mut last = 0usize;
    let valid = for_each_token(content, &mut |_, start, end| {
        stripped.push_str(&content[last..start]);
        last = end;
    });
    stripped.push_str(&content[last..]);
    !valid || stripped.contains("{{")
}

/// Port of `core/prompts/templates.ts::validatePromptTemplate`, including
/// the Ajv-compiling `validator()` step: a schema that will not compile
/// fails publish exactly like `AiToolRegistry.register` refusing it.
pub fn validate_prompt_template(template: &Value) -> Result<()> {
    let invalid_identity =
        || StoreError::InvalidInput("Invalid template identity or message count.".into());
    let messages = template["messages"]
        .as_array()
        .ok_or_else(invalid_identity)?;
    let non_empty = |field: &str| template[field].as_str().is_some_and(|v| !v.is_empty());
    if !non_empty("id") || !non_empty("revision") || messages.is_empty() || messages.len() > 100 {
        return Err(invalid_identity());
    }
    let schema = &template["argumentsSchema"];
    if schema["type"].as_str() != Some("object") {
        return Err(StoreError::InvalidInput(
            "Template arguments must be an object schema.".into(),
        ));
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("");
        let content = message["content"].as_str();
        if !matches!(role, "system" | "user" | "assistant")
            || content.is_none_or(|content| utf16_len(content) > 1_000_000)
        {
            return Err(StoreError::InvalidInput("Invalid template message.".into()));
        }
        let content = content.expect("checked above");
        let mut unknown: Option<String> = None;
        for_each_token(content, &mut |key, _, _| {
            if unknown.is_none() && !properties.contains_key(key) {
                unknown = Some(key.to_owned());
            }
        });
        if let Some(key) = unknown {
            return Err(StoreError::InvalidInput(format!(
                "Unknown template variable: {key}"
            )));
        }
        if has_leftover_expression(content) {
            return Err(StoreError::InvalidInput(
                "Unsupported template expression.".into(),
            ));
        }
    }
    compile_arguments_schema(schema)?;
    Ok(())
}

fn compile_arguments_schema(schema: &Value) -> Result<agentkit_contracts::SchemaValidator> {
    let mut compiled = schema.clone();
    if let Some(object) = compiled.as_object_mut() {
        object.insert("additionalProperties".into(), Value::Bool(false));
    }
    agentkit_contracts::compile_tool_schema(&compiled).map_err(|error| {
        StoreError::InvalidInput(format!("Template arguments schema is invalid: {error}"))
    })
}

/// Port of `templates.ts::stable`: JSON rendering with sorted object keys
/// and JavaScript number spellings (the boundary already carries parsed
/// JSON, so the TypeScript `undefined` refusal is unreachable here).
fn stable(value: &Value) -> Result<String> {
    match value {
        Value::Array(items) => {
            let rendered = items
                .iter()
                .map(stable)
                .collect::<Result<Vec<_>>>()?
                .join(",");
            Ok(format!("[{rendered}]"))
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort();
            let mut parts = Vec::with_capacity(keys.len());
            for key in keys {
                parts.push(format!(
                    "{}:{}",
                    agentkit_contracts::stringify_javascript_value(&Value::String(key.clone()))?,
                    stable(&object[key])?
                ));
            }
            Ok(format!("{{{}}}", parts.join(",")))
        }
        other => Ok(agentkit_contracts::stringify_javascript_value(other)?),
    }
}

/// Port of `templates.ts::renderPromptTemplate`.
pub fn render_prompt_template(template: &Value, selection: &Value) -> Result<Value> {
    if template["id"] != selection["id"] || template["revision"] != selection["revision"] {
        return Err(StoreError::InvalidInput(
            "Template revision mismatch.".into(),
        ));
    }
    validate_prompt_template(template)?;
    let arguments = &selection["arguments"];
    if !arguments.is_object() {
        return Err(StoreError::InvalidInput(
            "Template argument is not JSON.".into(),
        ));
    }
    let validator = compile_arguments_schema(&template["argumentsSchema"])?;
    if !validator.is_valid(arguments) {
        let rendered = validator
            .iter_errors(arguments)
            .map(|error| format!("{}: {error}", error.instance_path))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(StoreError::InvalidInput(format!(
            "Invalid template arguments: {rendered}"
        )));
    }
    let mut messages = Vec::new();
    for message in template["messages"].as_array().expect("validated") {
        let content = message["content"].as_str().expect("validated");
        let mut rendered = String::with_capacity(content.len());
        let mut last = 0usize;
        let mut missing: Option<String> = None;
        for_each_token(content, &mut |key, start, end| {
            if missing.is_some() {
                return;
            }
            let Some(value) = arguments.get(key) else {
                missing = Some(key.to_owned());
                return;
            };
            rendered.push_str(&content[last..start]);
            match value {
                Value::String(text) => rendered.push_str(text),
                other => rendered.push_str(&stable(other).unwrap_or_default()),
            }
            last = end;
        });
        if let Some(key) = missing {
            return Err(StoreError::InvalidInput(format!(
                "Missing template argument: {key}"
            )));
        }
        rendered.push_str(&content[last..]);
        let mut projected = message.as_object().expect("validated").clone();
        projected.insert("content".into(), Value::String(rendered));
        messages.push(Value::Object(projected));
    }
    let mut provenance = Map::new();
    provenance.insert("id".into(), template["id"].clone());
    provenance.insert("revision".into(), template["revision"].clone());
    if let Some(origin) = template.get("origin") {
        provenance.insert("origin".into(), origin.clone());
    }
    Ok(json!({ "provenance": Value::Object(provenance), "messages": messages }))
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// Port of `skills/skill-service.ts::parseSkillDocument` — the safe
/// frontmatter subset (scalars, literal/folded blocks, string metadata);
/// anything richer fails explicitly rather than being half-interpreted.
pub fn parse_skill_document(text: &str) -> Result<Value> {
    let invalid = |reason: &str| StoreError::InvalidInput(reason.to_owned());
    if text.len() > 1_000_000 {
        return Err(invalid("Skill exceeds byte limit."));
    }
    let normalized = text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let missing_frontmatter = || invalid("SKILL.md requires YAML frontmatter.");
    if lines.first() != Some(&"---") {
        return Err(missing_frontmatter());
    }
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| **line == "---")
        .map(|(index, _)| index)
        .ok_or_else(missing_frontmatter)?;
    let frontmatter = &lines[1..closing];
    let instructions = lines[closing + 1..].join("\n");

    let unquote = |value: &str| -> Result<String> {
        let trimmed = value.trim();
        if let Some(_rest) = trimmed.strip_prefix('"') {
            let parsed: Value = agentkit_contracts::parse_javascript_json(trimmed)
                .map_err(|_| invalid("Expected string metadata."))?;
            match parsed.as_str() {
                Some(text) => Ok(text.to_owned()),
                None => Err(invalid("Expected string metadata.")),
            }
        } else if let Some(rest) = trimmed.strip_prefix('\'') {
            if !rest.ends_with('\'') {
                return Err(invalid("Unterminated YAML string."));
            }
            Ok(rest[..rest.len() - 1].replace("''", "'"))
        } else if trimmed
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '&' | '*' | '!' | '[' | ']' | '{' | '}' | '>' | '|'))
        {
            Err(invalid("Unsupported YAML frontmatter value."))
        } else {
            Ok(strip_trailing_comment(trimmed))
        }
    };

    let mut fields: Map<String, Value> = Map::new();
    let mut metadata: Map<String, Value> = Map::new();
    let mut index = 0usize;
    while index < frontmatter.len() {
        let line = frontmatter[index];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            index += 1;
            continue;
        }
        let Some((key, value)) = split_frontmatter_pair(line) else {
            return Err(invalid("Unsupported YAML frontmatter syntax."));
        };
        if fields.contains_key(key) {
            return Err(invalid("Duplicate frontmatter key."));
        }
        let continues =
            |next: &str| next.starts_with(' ') || next.starts_with('\t') || next.trim().is_empty();
        if key == "metadata" && value.is_empty() {
            fields.insert(key.to_owned(), Value::String(String::new()));
            while index + 1 < frontmatter.len() && continues(frontmatter[index + 1]) {
                index += 1;
                let nested = frontmatter[index];
                if nested.trim().is_empty() || nested.trim_start().starts_with('#') {
                    continue;
                }
                let Some((nested_key, nested_value)) = split_metadata_pair(nested) else {
                    return Err(invalid("Invalid skill metadata."));
                };
                if metadata.contains_key(nested_key) {
                    return Err(invalid("Invalid skill metadata."));
                }
                metadata.insert(nested_key.to_owned(), Value::String(unquote(nested_value)?));
            }
        } else if matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let mut block: Vec<String> = Vec::new();
            while index + 1 < frontmatter.len() && continues(frontmatter[index + 1]) {
                index += 1;
                block.push(
                    frontmatter[index]
                        .strip_prefix("  ")
                        .unwrap_or(frontmatter[index])
                        .to_owned(),
                );
            }
            let joined = if value.starts_with('>') {
                block.join(" ")
            } else {
                block.join("\n")
            };
            fields.insert(key.to_owned(), Value::String(joined.trim().to_owned()));
        } else {
            fields.insert(key.to_owned(), Value::String(unquote(value)?));
        }
        index += 1;
    }
    let name = fields
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let description = fields
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let kebab = !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    if !kebab || description.is_empty() || utf16_len(&description) > 1024 {
        return Err(invalid("Invalid skill name or description."));
    }
    let string_field = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .map(|value| Value::String(value.to_owned()))
            .unwrap_or(Value::Null)
    };
    Ok(json!({
        "metadata": {
            "name": name,
            "description": description,
            "metadata": Value::Object(metadata),
            "license": string_field("license"),
            "compatibility": string_field("compatibility"),
            "allowedTools": string_field("allowed-tools"),
        },
        "instructions": instructions.trim(),
    }))
}

fn strip_trailing_comment(trimmed: &str) -> String {
    let bytes = trimmed.as_bytes();
    for index in 1..bytes.len() {
        if bytes[index] == b'#' && is_token_whitespace(bytes[index - 1]) {
            let mut start = index - 1;
            while start > 0 && is_token_whitespace(bytes[start - 1]) {
                start -= 1;
            }
            return trimmed[..start].to_owned();
        }
    }
    trimmed.to_owned()
}

/// `^([a-z][a-z0-9-]*):(?:\s+(.*))?$` — key plus optional value after
/// whitespace; `None` when the line does not match.
fn split_frontmatter_pair(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut index = 0usize;
    if index >= bytes.len() || !bytes[index].is_ascii_lowercase() {
        return None;
    }
    index += 1;
    while index < bytes.len()
        && (bytes[index].is_ascii_lowercase()
            || bytes[index].is_ascii_digit()
            || bytes[index] == b'-')
    {
        index += 1;
    }
    if index >= bytes.len() || bytes[index] != b':' {
        return None;
    }
    let key = &line[..index];
    let rest = &line[index + 1..];
    if rest.is_empty() {
        return Some((key, ""));
    }
    match rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_whitespace())
        .map(|(offset, _)| offset)
    {
        Some(0) => None,
        Some(offset) => Some((key, &rest[offset..])),
        None => Some((key, "")),
    }
}

/// `^ {2}([A-Za-z0-9_-]+):\s*(.*)$` — the two-space metadata item.
fn split_metadata_pair(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("  ")?;
    let bytes = rest.as_bytes();
    let mut index = 0usize;
    while index < bytes.len()
        && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' || bytes[index] == b'-')
    {
        index += 1;
    }
    if index == 0 || index >= bytes.len() || bytes[index] != b':' {
        return None;
    }
    let key = &rest[..index];
    let tail = &rest[index + 1..];
    let value_start = tail
        .char_indices()
        .find(|(_, c)| !c.is_ascii_whitespace())
        .map(|(offset, _)| offset)
        .unwrap_or(tail.len());
    Some((key, &tail[value_start..]))
}

/// Port of `skillContentHash`: lowercase sha-256 hex over the raw bytes.
pub fn skill_content_hash(text: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Port of `SkillService.activate`'s reference-path containment rule.
fn validate_reference_path(path: &str) -> Result<()> {
    let refused =
        || StoreError::InvalidInput("Skill reference must remain inside its root.".into());
    if path.is_empty() || path.contains('\\') || path.starts_with('/') {
        return Err(refused());
    }
    if path
        .split('/')
        .any(|segment| segment == ".." || segment == "." || segment.is_empty())
    {
        return Err(refused());
    }
    Ok(())
}

fn connector_requirements(document: &Value) -> Vec<String> {
    document["metadata"]["metadata"]["requires-connectors"]
        .as_str()
        .unwrap_or("")
        .split(',')
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

fn skill_entry(record: &Value) -> Value {
    json!({
        "id": record["id"],
        "revision": record["revision"],
        "sourceId": record["sourceId"],
        "path": record["path"],
        "name": record["name"],
        "description": record["description"],
        "connectorRequirements": record["connectorRequirements"],
    })
}

// ---------------------------------------------------------------------------
// Connections (non-secret configuration only)
// ---------------------------------------------------------------------------

fn check_optional_string_map(object: &Map<String, Value>, field: &str) -> Option<String> {
    let value = object.get(field)?;
    let Some(map) = value.as_object() else {
        return Some(format!("`{field}` must be an object."));
    };
    for (key, entry) in map {
        if !entry.is_string() {
            return Some(format!("`{field}.{key}` must be a string."));
        }
    }
    None
}

fn check_optional_boolean(object: &Map<String, Value>, field: &str) -> Option<String> {
    match object.get(field) {
        None | Some(Value::Bool(_)) => None,
        Some(_) => Some(format!("`{field}` must be a boolean.")),
    }
}

fn check_required_non_empty(object: &Map<String, Value>, field: &str) -> Option<String> {
    match object.get(field).and_then(Value::as_str) {
        None => Some(format!("`{field}` must be a string.")),
        Some(value) if value.trim().is_empty() => Some(format!("`{field}` must not be empty.")),
        Some(_) => None,
    }
}

fn is_server_alias(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Port of `normalizeServerAlias`: trim, then `^[a-z][a-z0-9-]*$`.
pub fn normalize_server_alias(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if !is_server_alias(trimmed) {
        return Err(StoreError::InvalidInput(format!(
            "MCP server alias \"{value}\" is invalid; expected ^[a-z][a-z0-9-]*$."
        )));
    }
    Ok(trimmed.to_owned())
}

fn check_transport(transport: Option<&Value>) -> Option<String> {
    let Some(transport) = transport else {
        return Some("`transport` is required.".to_owned());
    };
    let Some(object) = transport.as_object() else {
        return Some("`transport` must be an object.".to_owned());
    };
    match object.get("kind").and_then(Value::as_str) {
        Some("stdio") => {
            if let Some(issue) = check_required_non_empty(object, "command")
                .or_else(|| check_optional_string_map(object, "env"))
            {
                return Some(format!("`transport`: {issue}"));
            }
            if let Some(args) = object.get("args") {
                let valid = args
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string));
                if !valid {
                    return Some("`transport.args` must be an array of strings.".to_owned());
                }
            }
            None
        }
        Some("http") => check_required_non_empty(object, "url")
            .or_else(|| check_optional_string_map(object, "headers"))
            .map(|issue| format!("`transport`: {issue}")),
        _ => Some("`transport.kind` must be \"stdio\" or \"http\".".to_owned()),
    }
}

fn check_optional_number(
    object: &Map<String, Value>,
    field: &str,
    integer: bool,
    min: f64,
    max: f64,
) -> Option<String> {
    let value = object.get(field)?;
    let Some(number) = value.as_f64() else {
        return Some(format!("`{field}` must be a number."));
    };
    if (integer && number.fract() != 0.0) || number < min || number > max {
        return Some(format!("`{field}` must be between {min} and {max}."));
    }
    None
}

fn check_resilience(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let Some(object) = value.as_object() else {
        return Some("`resilience` must be an object.".to_owned());
    };
    const MS_FIELDS: [&str; 5] = [
        "requestTimeoutMs",
        "connectTimeoutMs",
        "connectBackoffBaseMs",
        "connectBackoffMaxMs",
        "circuitOpenMs",
    ];
    const ATTEMPT_FIELDS: [&str; 2] = ["maxConnectAttempts", "reconnectMaxAttempts"];
    for key in object.keys() {
        if !MS_FIELDS.contains(&key.as_str())
            && !ATTEMPT_FIELDS.contains(&key.as_str())
            && key != "reconnectBackoffFactor"
            && key != "retryTimeouts"
        {
            return Some(format!(
                "`resilience.{key}` is not a known resilience option."
            ));
        }
    }
    for field in MS_FIELDS {
        if let Some(issue) = check_optional_number(object, field, true, 0.0, 600_000.0) {
            return Some(format!("`resilience`: {issue}"));
        }
    }
    for field in ATTEMPT_FIELDS {
        if let Some(issue) = check_optional_number(object, field, true, 1.0, 20.0) {
            return Some(format!("`resilience`: {issue}"));
        }
    }
    if let Some(issue) = check_optional_number(object, "reconnectBackoffFactor", false, 1.0, 20.0) {
        return Some(format!("`resilience`: {issue}"));
    }
    if let Some(issue) = check_optional_boolean(object, "retryTimeouts") {
        return Some(format!("`resilience`: {issue}"));
    }
    None
}

/// Port of `validateCreateMcpServerRequest` (transport-http) over a
/// connection's `config`, extended with the binding's deny-all `secretRefs`
/// refusal: this store has no secret resolution, so a reference is refused
/// at the door rather than accepted-and-dropped. Returns the normalized
/// alias the record must carry.
pub fn validate_connection_config(config: &Value) -> Result<String> {
    let object = config
        .as_object()
        .ok_or_else(|| StoreError::InvalidInput("`config` must be an object.".to_owned()))?;
    if let Some(issue) = [
        check_required_non_empty(object, "alias"),
        object
            .get("alias")
            .and_then(Value::as_str)
            .filter(|alias| !alias.trim().is_empty() && !is_server_alias(alias.trim()))
            .map(|_| "`alias` must match ^[a-z][a-z0-9-]*$.".to_owned()),
        check_transport(object.get("transport")),
        check_optional_string_map(object, "secretRefs"),
        check_optional_boolean(object, "enabled"),
        check_optional_string_map(object, "toolAliases"),
        check_resilience(object.get("resilience")),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return Err(StoreError::InvalidInput(issue));
    }
    if let Some(refs) = object.get("secretRefs").and_then(Value::as_object)
        && !refs.is_empty()
    {
        return Err(StoreError::InvalidInput(format!(
            "{SECRET_REF_MARKER}{} secretRef entries were provided and the native binding resolves none.",
            refs.len()
        )));
    }
    normalize_server_alias(object["alias"].as_str().expect("validated"))
}

/// The connection list projection: credential-shaped transport values are
/// redacted on EVERY read path (`***`), so a managed connection record can
/// never echo a literal back through the binding or REST.
pub fn redact_connection(record: &mut Value) {
    let transport = record
        .get_mut("config")
        .and_then(|config| config.get_mut("transport"))
        .and_then(Value::as_object_mut);
    let Some(transport) = transport else {
        return;
    };
    for field in ["env", "headers"] {
        if let Some(bag) = transport
            .get_mut(field)
            .and_then(|value| value.as_object_mut())
        {
            for entry in bag.values_mut() {
                *entry = Value::String("***".to_owned());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Engine: one bounded worker, one table family
// ---------------------------------------------------------------------------

fn persisted(db: &Connection, value: &Value) -> Result<String> {
    let normalized = agentkit_contracts::parse_javascript_json(&serde_json::to_string(value)?)?;
    let serialized = if db.path().is_none_or(str::is_empty) {
        serde_json::to_string(&normalized)?
    } else {
        agentkit_contracts::stringify_javascript_value(&normalized)?
    };
    // Re-read what will be stored: a record this store cannot parse back
    // never reaches SQLite.
    agentkit_contracts::parse_javascript_json(&serialized)?;
    Ok(serialized)
}

fn read_stored(text: &str) -> Result<Value> {
    Ok(agentkit_contracts::parse_javascript_json(text)?)
}

fn required_text<'a>(input: &'a Value, field: &str, what: &str) -> Result<&'a str> {
    input[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| StoreError::InvalidInput(format!("{what} must be a non-empty string")))
}

impl SqliteStore {
    // -- prompt templates ---------------------------------------------------

    /// `PromptTemplateService.publish`: validate, then insert. Revisions are
    /// immutable — an existing `(id, revision)` (live or tombstoned) refuses
    /// the publish exactly like the SQLite primary key does in TypeScript.
    pub async fn publish_prompt_template(&self, template: Value) -> Result<Value> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            validate_prompt_template(&template)?;
            let serialized = persisted(&transaction, &template)?;
            let duplicate = || {
                StoreError::Duplicate(format!(
                    "Prompt template revision already exists: {}@{}",
                    template["id"], template["revision"]
                ))
            };
            let inserted = transaction.execute(
                "INSERT INTO prompt_templates (id, revision, template) VALUES (?1, ?2, ?3)",
                params![
                    template["id"].as_str().expect("validated"),
                    template["revision"].as_str().expect("validated"),
                    serialized
                ],
            );
            match inserted {
                Ok(1) => {
                    transaction.commit()?;
                    Ok(template)
                }
                Ok(_) => Err(duplicate()),
                Err(rusqlite::Error::SqliteFailure(failure, _))
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    Err(duplicate())
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    /// `PromptTemplateStore.list`: `(id, revision)` order; tombstoned
    /// (erased-body) revisions read as absence, exactly like the shared
    /// SQLite adapter's `null`-body rows.
    pub async fn list_prompt_templates(&self) -> Result<Vec<Value>> {
        self.call(move |db| {
            let mut statement =
                db.prepare("SELECT template FROM prompt_templates ORDER BY id, revision")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut templates = Vec::new();
            for row in rows {
                let value = read_stored(&row?)?;
                if !value.is_null() {
                    templates.push(value);
                }
            }
            Ok(templates)
        })
        .await
    }

    /// `PromptTemplateStore.get`: the stored template or absence (an
    /// unknown or tombstoned revision).
    pub async fn get_prompt_template(&self, id: String, revision: String) -> Result<Option<Value>> {
        self.call(move |db| {
            let row: Option<String> = db
                .query_row(
                    "SELECT template FROM prompt_templates WHERE id = ?1 AND revision = ?2",
                    params![id, revision],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(text) = row else { return Ok(None) };
            let value = read_stored(&text)?;
            Ok((!value.is_null()).then_some(value))
        })
        .await
    }

    /// `PromptTemplateStore.delete`: erases the body while permanently
    /// reserving the immutable revision (the primary key stays). Unknown
    /// revisions are a no-op, exactly like the adapter's UPDATE.
    pub async fn delete_prompt_template(&self, id: String, revision: String) -> Result<()> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            transaction.execute(
                "UPDATE prompt_templates SET template = 'null' WHERE id = ?1 AND revision = ?2",
                params![id, revision],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// `PromptTemplateService.render`: a missing revision is the service's
    /// "Prompt template revision not found." — a `NotFound`, not a silent
    /// empty render.
    pub async fn render_prompt_template_record(&self, selection: Value) -> Result<Value> {
        self.call(move |db| {
            let id = selection["id"].as_str().unwrap_or("").to_owned();
            let revision = selection["revision"].as_str().unwrap_or("").to_owned();
            let row: Option<String> = db
                .query_row(
                    "SELECT template FROM prompt_templates WHERE id = ?1 AND revision = ?2",
                    params![id, revision],
                    |row| row.get(0),
                )
                .optional()?;
            let template = match row {
                Some(text) => read_stored(&text)?,
                None => Value::Null,
            };
            if template.is_null() {
                return Err(StoreError::NotFound(
                    "Prompt template revision not found.".into(),
                ));
            }
            render_prompt_template(&template, &selection)
        })
        .await
    }

    // -- managed skills -------------------------------------------------------

    /// `SkillService.catalog` over store-published documents: parse the
    /// frontmatter subset, content-hash the RAW text (the revision, exactly
    /// like `skillContentHash`), and store the document under
    /// `(source, path)`. Re-publishing a path replaces the document — the
    /// store-managed analogue of editing a file in a configured skill root:
    /// the content-addressed revision changes and stale selections fail
    /// activation with the reference's own refusal.
    pub async fn publish_skill(
        &self,
        source: String,
        path: String,
        content: String,
    ) -> Result<Value> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            if source.is_empty() || source.contains(':') {
                return Err(StoreError::InvalidInput(
                    "Skill source must be a non-empty string without ':'".into(),
                ));
            }
            validate_reference_path(&path)?;
            let document = parse_skill_document(&content)?;
            let entry = json!({
                "id": format!("{source}:{path}"),
                "revision": skill_content_hash(&content),
                "sourceId": source,
                "path": path,
                "name": document["metadata"]["name"],
                "description": document["metadata"]["description"],
                "connectorRequirements": connector_requirements(&document),
                "instructions": document["instructions"],
                "content": content,
            });
            let serialized = persisted(&transaction, &entry)?;
            transaction.execute(
                "INSERT INTO managed_skills (source_id, path, revision, record) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source_id, path) DO UPDATE SET revision = excluded.revision, record = excluded.record",
                params![source, path, entry["revision"].as_str().expect("hashed"), serialized],
            )?;
            transaction.commit()?;
            Ok(skill_entry(&entry))
        })
        .await
    }

    /// `SkillService.catalog`: catalog entries in publish order (the store
    /// analogue of source registration order), without document bodies.
    pub async fn list_skills(&self) -> Result<Vec<Value>> {
        self.call(move |db| {
            let mut statement = db.prepare("SELECT record FROM managed_skills ORDER BY rowid")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| Ok(skill_entry(&read_stored(&row?)?)))
                .collect()
        })
        .await
    }

    /// One catalog entry by its `source:path` identity.
    pub async fn get_skill(&self, id: String) -> Result<Option<Value>> {
        self.call(move |db| {
            let Some(record) = select_skill(db, &id)? else {
                return Ok(None);
            };
            Ok(Some(skill_entry(&record)))
        })
        .await
    }

    /// Remove a skill document; unknown identities are a no-op.
    pub async fn delete_skill(&self, id: String) -> Result<()> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            let Some((source, path)) = split_skill_id(&id) else {
                return Ok(());
            };
            transaction.execute(
                "DELETE FROM managed_skills WHERE source_id = ?1 AND path = ?2",
                params![source, path],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    // -- connections ------------------------------------------------------------

    /// `ConnectionService.save` (the non-secret configuration leg): one
    /// atomic compare-and-swap with the reference's validation order —
    /// revision check, identity rules, immutable account/workspace,
    /// normalized-alias uniqueness, request validation, then the CAS write.
    /// Lifecycle (connect/reconnect/test) is the documented MCP-OAuth
    /// reduction and is refused above this layer, never here.
    pub async fn save_connection(
        &self,
        record: Value,
        expected_revision: Option<i64>,
    ) -> Result<Value> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            // The reference's exact order: compare-and-swap first, then the
            // identity rules, then alias normalization + cross-account
            // uniqueness, then the request validator (`definition.validate`).
            let revision = record["revision"]
                .as_i64()
                .filter(|value| *value >= 1)
                .ok_or_else(|| {
                    StoreError::InvalidInput("Connection revision must be an integer >= 1.".into())
                })?;
            let stale = || StoreError::InvalidTransition("Connection revision changed.".into());
            let id = record["id"].as_str().unwrap_or("").to_owned();
            let existing: Option<(i64, String)> = transaction
                .query_row(
                    "SELECT revision, record FROM connector_connections WHERE id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let current = existing.as_ref().map(|(revision, _)| *revision);
            if current != expected_revision || revision != expected_revision.unwrap_or(0) + 1 {
                return Err(stale());
            }
            if record["id"].as_str().is_none_or(|id| id.is_empty())
                || record["accountId"]
                    .as_str()
                    .is_none_or(|account| account.is_empty())
            {
                return Err(StoreError::InvalidInput(
                    "Connection and account identities are required.".into(),
                ));
            }
            required_text(&record, "connectorId", "Connector id")?;
            if let Some(workspace) = record.get("workspaceId")
                && !workspace.is_null()
                && !workspace.is_string()
            {
                return Err(StoreError::InvalidInput(
                    "Connection and account identities are required.".into(),
                ));
            }
            if let Some((_, text)) = existing.as_ref() {
                let old = read_stored(text)?;
                for field in ["accountId", "connectorId", "workspaceId"] {
                    if old.get(field) != record.get(field)
                        && !(old.get(field).is_none_or(Value::is_null)
                            && record.get(field).is_none_or(Value::is_null))
                    {
                        return Err(StoreError::InvalidInput(
                            "Account and workspace identity are immutable; create a separate connection."
                                .into(),
                        ));
                    }
                }
            }
            let alias = normalize_server_alias(
                record["config"]["alias"]
                    .as_str()
                    .filter(|alias| !alias.trim().is_empty())
                    .ok_or_else(|| {
                        StoreError::InvalidInput("`alias` must be a string.".to_owned())
                    })?,
            )?;
            let mut conflicts = false;
            {
                let mut statement = transaction
                    .prepare("SELECT record FROM connector_connections WHERE id != ?1")?;
                let rows = statement.query_map(params![id], |row| row.get::<_, String>(0))?;
                for row in rows {
                    let other = read_stored(&row?)?;
                    let normalized = normalize_server_alias(
                        other["config"]["alias"].as_str().unwrap_or_default(),
                    )?;
                    if normalized == alias {
                        conflicts = true;
                        break;
                    }
                }
            }
            if conflicts {
                return Err(StoreError::InvalidInput(
                    "Connection aliases must be unique across accounts.".into(),
                ));
            }
            validate_connection_config(&record["config"])?;
            let mut draft = record
                .as_object()
                .expect("validated")
                .clone();
            draft.remove("status");
            draft.remove("errorCode");
            let mut config = draft
                .get("config")
                .and_then(Value::as_object)
                .expect("validated")
                .clone();
            config.insert("alias".into(), Value::String(alias));
            draft.insert("config".into(), Value::Object(config));
            draft.insert("status".into(), Value::String("disconnected".into()));
            let saved = Value::Object(draft);
            let serialized = persisted(&transaction, &saved)?;
            let changed = match expected_revision {
                None => transaction.execute(
                    "INSERT INTO connector_connections (id, revision, record) VALUES (?1, ?2, ?3)",
                    params![id, revision, serialized],
                )?,
                Some(expected) => transaction.execute(
                    "UPDATE connector_connections SET revision = ?2, record = ?3 WHERE id = ?1 AND revision = ?4",
                    params![id, revision, serialized, expected],
                )?,
            };
            if changed != 1 {
                return Err(stale());
            }
            transaction.commit()?;
            let mut saved = saved;
            redact_connection(&mut saved);
            Ok(saved)
        })
        .await
    }

    /// `ConnectionStore.list`: records by id (the shared adapter's order),
    /// credential-shaped values redacted.
    pub async fn list_connections(&self) -> Result<Vec<Value>> {
        self.call(move |db| {
            let mut statement =
                db.prepare("SELECT record FROM connector_connections ORDER BY id")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| {
                let mut record = read_stored(&row?)?;
                redact_connection(&mut record);
                Ok(record)
            })
            .collect()
        })
        .await
    }

    /// `ConnectionStore.get`: the record or absence, credential-shaped
    /// values redacted.
    pub async fn get_connection(&self, id: String) -> Result<Option<Value>> {
        self.call(move |db| {
            let row: Option<String> = db
                .query_row(
                    "SELECT record FROM connector_connections WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(text) = row else { return Ok(None) };
            let mut record = read_stored(&text)?;
            redact_connection(&mut record);
            Ok(Some(record))
        })
        .await
    }

    /// `ConnectionService.remove` (the store leg): local deletion always
    /// takes effect; there is no secret to scrub (this store holds none) and
    /// remote revocation is the documented OAuth reduction refused above
    /// this layer. Unknown ids are a no-op, exactly like the adapter.
    pub async fn delete_connection(&self, id: String) -> Result<()> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            transaction.execute(
                "DELETE FROM connector_connections WHERE id = ?1",
                params![id],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    // -- run-content records -----------------------------------------------------

    /// The persisted analogue of `SkillService`'s `recordResource` hook:
    /// one `{skill, path, revision}` reference per activated-skill read.
    pub async fn record_run_content_resource(
        &self,
        skill: Value,
        path: String,
        revision: String,
    ) -> Result<()> {
        self.call(move |db| {
            let transaction = atomic(db)?;
            required_text(&skill, "id", "Skill id")?;
            required_text(&skill, "revision", "Skill revision")?;
            validate_reference_path(&path)?;
            if !is_hex64(&revision) {
                return Err(StoreError::InvalidInput(
                    "Skill reference revision must be a sha-256 hex digest".into(),
                ));
            }
            let record = json!({ "skill": skill, "path": path, "revision": revision });
            let serialized = persisted(&transaction, &record)?;
            transaction.execute(
                "INSERT INTO run_content_records (record) VALUES (?1)",
                params![serialized],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Recorded skill references, oldest first; optionally one skill's only.
    pub async fn list_run_content_resources(&self, skill_id: Option<String>) -> Result<Vec<Value>> {
        self.call(move |db| {
            let mut records = Vec::new();
            match &skill_id {
                Some(skill_id) => {
                    let mut statement = db.prepare(
                        "SELECT record FROM run_content_records \
                         WHERE json_extract(record, '$.skill.id') = ?1 ORDER BY rowid",
                    )?;
                    let rows =
                        statement.query_map(params![skill_id], |row| row.get::<_, String>(0))?;
                    for row in rows {
                        records.push(read_stored(&row?)?);
                    }
                }
                None => {
                    let mut statement =
                        db.prepare("SELECT record FROM run_content_records ORDER BY rowid")?;
                    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                    for row in rows {
                        records.push(read_stored(&row?)?);
                    }
                }
            }
            Ok(records)
        })
        .await
    }

    /// `createRunContentProvider(...).resolve(...)`: template messages plus
    /// one context block per authorized skill. `tolerate_unavailable`
    /// mirrors the provider's two modes — with the TypeScript
    /// `onSkillUnavailable` callback a failing skill is reported and
    /// skipped; without it the reference throws. The JSON boundary carries
    /// the same choice as a boolean plus the reported `unavailable` list.
    pub async fn resolve_run_content(
        &self,
        selection: Value,
        authorized_connectors: Vec<String>,
        tolerate_unavailable: bool,
    ) -> Result<Value> {
        self.call(move |db| {
            let mut messages = Vec::new();
            let mut blocks = Vec::new();
            let mut unavailable = Vec::new();
            if let Some(template) = selection.get("template").filter(|v| !v.is_null()) {
                let rendered = render_prompt_template(&fetch_template(db, template)?, template)?;
                messages = rendered["messages"].as_array().expect("shape").clone();
            }
            let skills = match selection.get("skills") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items.clone(),
                Some(_) => {
                    return Err(StoreError::InvalidInput("Invalid skill selection.".into()));
                }
            };
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            if skills.len() > 20 {
                return Err(StoreError::InvalidInput("Invalid skill selection.".into()));
            }
            for skill in &skills {
                let id = skill["id"].as_str().unwrap_or("").to_owned();
                if id.is_empty() || !seen.insert(id.clone()) {
                    return Err(StoreError::InvalidInput("Invalid skill selection.".into()));
                }
            }
            for skill in &skills {
                let id = skill["id"].as_str().expect("validated");
                let revision = skill["revision"].as_str().unwrap_or("");
                let failure: Result<String> = (|| -> Result<String> {
                    let record = select_skill(db, id)?
                        .ok_or_else(|| StoreError::InvalidInput(
                            "Selected skill revision changed or disappeared.".into(),
                        ))?;
                    if record["revision"].as_str() != Some(revision) {
                        return Err(StoreError::InvalidInput(
                            "Selected skill revision changed or disappeared.".into(),
                        ));
                    }
                    let requirements = record["connectorRequirements"]
                        .as_array()
                        .expect("stored shape")
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>();
                    if requirements
                        .iter()
                        .any(|need| !authorized_connectors.iter().any(|grant| grant == need))
                    {
                        return Err(StoreError::InvalidInput(
                            "Skill requires a selected authorized connector.".into(),
                        ));
                    }
                    // The stored revision is content-hashed at publish; the
                    // reference re-hashes between catalog and read, so the
                    // engine re-checks its own table honestly.
                    if skill_content_hash(record["content"].as_str().expect("stored shape"))
                        != revision
                    {
                        return Err(StoreError::InvalidInput(
                            "Skill changed during activation.".into(),
                        ));
                    }
                    blocks.push(json!({
                        "id": format!("skill:{}@{}", record["id"].as_str().expect("stored"), revision),
                        "title": format!("Skill: {}", record["name"].as_str().expect("stored")),
                        "content": record["instructions"],
                        "priority": 100,
                    }));
                    Ok(String::new())
                })();
                if let Err(error) = failure {
                    if !tolerate_unavailable {
                        return Err(error);
                    }
                    unavailable.push(json!({ "id": id, "reason": error.to_string() }));
                }
            }
            Ok(json!({ "blocks": blocks, "messages": messages, "unavailable": unavailable }))
        })
        .await
    }
}

fn fetch_template(db: &Connection, selection: &Value) -> Result<Value> {
    let row: Option<String> = db
        .query_row(
            "SELECT template FROM prompt_templates WHERE id = ?1 AND revision = ?2",
            params![
                selection["id"].as_str().unwrap_or(""),
                selection["revision"].as_str().unwrap_or("")
            ],
            |row| row.get(0),
        )
        .optional()?;
    let template = match row {
        Some(text) => read_stored(&text)?,
        None => Value::Null,
    };
    if template.is_null() {
        return Err(StoreError::NotFound(
            "Prompt template revision not found.".into(),
        ));
    }
    Ok(template)
}

fn split_skill_id(id: &str) -> Option<(&str, &str)> {
    let (source, path) = id.split_once(':')?;
    if source.is_empty() || path.is_empty() {
        return None;
    }
    Some((source, path))
}

fn select_skill(db: &Connection, id: &str) -> Result<Option<Value>> {
    let Some((source, path)) = split_skill_id(id) else {
        return Ok(None);
    };
    let row: Option<String> = db
        .query_row(
            "SELECT record FROM managed_skills WHERE source_id = ?1 AND path = ?2",
            params![source, path],
            |row| row.get(0),
        )
        .optional()?;
    row.map(|text| read_stored(&text)).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(id: &str, revision: &str, content: &str) -> Value {
        json!({
            "id": id,
            "revision": revision,
            "description": "test",
            "argumentsSchema": { "type": "object", "properties": { "name": { "type": "string" } } },
            "messages": [{ "role": "user", "content": content }],
        })
    }

    #[test]
    fn template_validation_mirrors_the_reference_refusals() {
        assert!(validate_prompt_template(&template("t", "1", "Hello {{name}}")).is_ok());
        assert!(
            validate_prompt_template(&template("t", "1", "Hello {{missing}}")).is_err(),
            "unknown variable"
        );
        assert!(
            validate_prompt_template(&template("t", "1", "Hello {{name {{other}}")).is_err(),
            "leftover expression"
        );
        let mut bad = template("t", "1", "x");
        bad["argumentsSchema"]["type"] = json!("array");
        assert!(validate_prompt_template(&bad).is_err());
        let mut empty = template("", "1", "x");
        empty["id"] = json!("");
        assert!(validate_prompt_template(&empty).is_err());
    }

    #[test]
    fn render_substitutes_strings_and_stable_json() {
        let mut subject = template("t", "1", "{{name}} / {{count}} / {{flag}} / {{deep}}");
        let schema = subject["argumentsSchema"].as_object_mut().expect("schema");
        let properties = schema["properties"].as_object_mut().expect("properties");
        properties.insert(
            "count".into(),
            json!({ "anyOf": [{ "type": "number" }, { "type": "null" }] }),
        );
        properties.insert(
            "flag".into(),
            json!({ "anyOf": [{ "type": "boolean" }, { "type": "null" }] }),
        );
        properties.insert("deep".into(), json!({}));
        let rendered = render_prompt_template(
            &subject,
            &json!({
                "id": "t",
                "revision": "1",
                "arguments": { "name": "a\"b", "count": 2, "flag": true, "deep": { "b": 1, "a": [1, "x"] } },
            }),
        )
        .expect("renders");
        assert_eq!(
            rendered["messages"][0]["content"],
            json!("a\"b / 2 / true / {\"a\":[1,\"x\"],\"b\":1}")
        );
        assert_eq!(rendered["provenance"]["id"], json!("t"));
    }

    #[test]
    fn skill_frontmatter_matches_the_reference_subset() {
        let document = parse_skill_document(
            "---\nname: my-skill\ndescription: \"A 'quoted' skill\"\nlicense: MIT\nmetadata:\n  requires-connectors: github, jira\nallowed-tools: read\n---\n\nDo the thing\n",
        )
        .expect("parses");
        assert_eq!(document["metadata"]["name"], json!("my-skill"));
        assert_eq!(
            document["metadata"]["description"],
            json!("A 'quoted' skill")
        );
        assert_eq!(document["metadata"]["license"], json!("MIT"));
        assert_eq!(document["metadata"]["allowedTools"], json!("read"));
        assert_eq!(
            connector_requirements(&document),
            vec!["github".to_owned(), "jira".to_owned()]
        );
        assert_eq!(document["instructions"], json!("Do the thing"));
        for case in [
            "no frontmatter",
            "---\nname: Bad_Name\ndescription: x\n---\n",
            "---\nname: a\ndescription: x\nname: b\n---\n",
            "---\nname: a\ndescription: x\nkey: &anchor\n---\n",
        ] {
            assert!(parse_skill_document(case).is_err(), "{case}");
        }
    }

    #[test]
    fn skill_hash_is_lowercase_sha256_of_the_raw_text() {
        assert_eq!(
            skill_content_hash("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn connection_config_validation_mirrors_the_validator() {
        let good = json!({
            "alias": " github ",
            "transport": { "kind": "http", "url": "https://example.test/mcp" },
            "enabled": true,
            "resilience": { "requestTimeoutMs": 1000, "reconnectBackoffFactor": 1.5 },
        });
        assert_eq!(validate_connection_config(&good).expect("valid"), "github");
        assert!(
            validate_connection_config(
                &json!({ "alias": "Bad Alias", "transport": { "kind": "http", "url": "u" } })
            )
            .is_err()
        );
        assert!(
            validate_connection_config(&json!({ "alias": "ok", "transport": { "kind": "grpc" } }))
                .is_err()
        );
        assert!(validate_connection_config(&json!({ "alias": "ok", "transport": { "kind": "stdio", "command": "x", "env": { "A": 1 } } })).is_err());
        assert!(
            validate_connection_config(&json!({
                "alias": "ok",
                "transport": { "kind": "stdio", "command": "x" },
                "resilience": { "requestTimeouMs": 5 },
            }))
            .is_err()
        );
        let denied = validate_connection_config(&json!({
                "alias": "ok",
                "transport": { "kind": "stdio", "command": "x" },
                "secretRefs": { "token": "vault/ref" },
        }))
        .expect_err("deny-all");
        assert!(denied.to_string().contains(SECRET_REF_MARKER));
        // An empty bag carries no entries and stays allowed, like TypeScript.
        assert!(
            validate_connection_config(&json!({
                "alias": "ok",
                "transport": { "kind": "stdio", "command": "x" },
                "secretRefs": {},
            }))
            .is_ok()
        );
    }

    #[test]
    fn reference_paths_follow_the_containment_rule() {
        assert!(validate_reference_path("references/guide.md").is_ok());
        for case in ["", "a\\b", "/abs", "a/../b", "./a", "a//b", "a/./b"] {
            assert!(validate_reference_path(case).is_err(), "{case}");
        }
        assert!(is_hex64(&skill_content_hash("x")));
        assert!(!is_hex64(&skill_content_hash("x").to_uppercase()));
    }

    #[tokio::test]
    async fn managed_content_lifecycles_survive_reopen() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("managed.db");
        let store = SqliteStore::open(&path, 16).expect("open");

        // Templates: publish, list order, immutable revision, tombstone.
        store
            .publish_prompt_template(template("b", "1", "B {{name}}"))
            .await
            .expect("publish");
        store
            .publish_prompt_template(template("a", "2", "A2"))
            .await
            .expect("publish");
        store
            .publish_prompt_template(template("a", "1", "A1"))
            .await
            .expect("publish");
        let listed = store.list_prompt_templates().await.expect("list");
        let ids: Vec<&str> = listed
            .iter()
            .map(|t| t["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["a", "a", "b"]);
        let duplicate = store
            .publish_prompt_template(template("a", "1", "again"))
            .await
            .expect_err("immutable revision");
        assert!(matches!(duplicate, StoreError::Duplicate(_)));
        let rendered = store
            .render_prompt_template_record(
                json!({ "id": "b", "revision": "1", "arguments": { "name": "x" } }),
            )
            .await
            .expect("render");
        assert_eq!(rendered["messages"][0]["content"], json!("B x"));
        store
            .delete_prompt_template("a".into(), "1".into())
            .await
            .expect("delete");
        assert!(
            store
                .get_prompt_template("a".into(), "1".into())
                .await
                .expect("get")
                .is_none()
        );
        assert_eq!(store.list_prompt_templates().await.expect("list").len(), 2);
        assert!(matches!(
            store
                .publish_prompt_template(template("a", "1", "reserved"))
                .await
                .expect_err("tombstone reserves"),
            StoreError::Duplicate(_)
        ));

        // Connections: CAS, identity rules, alias uniqueness, redaction.
        let record = |revision: i64, alias: &str| {
            json!({
                "id": "c1",
                "connectorId": "github",
                "accountId": "acc",
                "revision": revision,
                "config": {
                    "alias": alias,
                    "transport": { "kind": "stdio", "command": "x", "env": { "TOKEN": "secret" } },
                },
            })
        };
        let stale = store
            .save_connection(record(1, "github"), Some(9))
            .await
            .expect_err("stale create");
        assert!(matches!(stale, StoreError::InvalidTransition(_)));
        let saved = store
            .save_connection(record(1, " github "), None)
            .await
            .expect("create");
        assert_eq!(saved["status"], json!("disconnected"));
        assert_eq!(saved["config"]["alias"], json!("github"));
        assert_eq!(saved["config"]["transport"]["env"]["TOKEN"], json!("***"));
        assert!(
            store
                .save_connection(record(3, "github"), Some(1))
                .await
                .is_err_and(|error| error.to_string().contains("revision"))
        );
        assert!(
            store
                .save_connection(record(2, "github"), Some(1))
                .await
                .expect("bump")["config"]
                .get("secretRefs")
                .is_none()
        );
        assert!(
            store
                .save_connection(
                    {
                        let mut moved = record(3, "github");
                        moved["accountId"] = json!("other");
                        moved
                    },
                    Some(2),
                )
                .await
                .is_err_and(|error| error.to_string().contains("immutable"))
        );
        let mut competing = record(1, "github");
        competing["id"] = json!("c2");
        assert!(
            store
                .save_connection(competing, None)
                .await
                .is_err_and(|error| error.to_string().contains("alias"))
        );

        // Skills + run content.
        let skill_text = "---\nname: reviewer\ndescription: reviews\nmetadata:\n  requires-connectors: github\n---\nbody text\n";
        let entry = store
            .publish_skill("docs".into(), "reviewer".into(), skill_text.into())
            .await
            .expect("publish skill");
        assert_eq!(entry["id"], json!("docs:reviewer"));
        assert_eq!(entry["revision"], json!(skill_content_hash(skill_text)));
        let resolved = store
            .resolve_run_content(
                json!({ "skills": [{ "id": "docs:reviewer", "revision": entry["revision"] }] }),
                vec!["github".into()],
                false,
            )
            .await
            .expect("resolve");
        assert_eq!(
            resolved["blocks"][0]["id"],
            json!(format!(
                "skill:docs:reviewer@{}",
                skill_content_hash(skill_text)
            ))
        );
        assert_eq!(resolved["blocks"][0]["priority"], json!(100));
        assert!(
            store
                .resolve_run_content(
                    json!({ "skills": [{ "id": "docs:reviewer", "revision": entry["revision"] }] }),
                    vec![],
                    false,
                )
                .await
                .is_err_and(|error| error.to_string().contains("authorized connector"))
        );
        let tolerated = store
            .resolve_run_content(
                json!({ "skills": [{ "id": "docs:reviewer", "revision": "stale" }] }),
                vec![],
                true,
            )
            .await
            .expect("tolerated");
        assert_eq!(tolerated["unavailable"][0]["id"], json!("docs:reviewer"));
        store
            .record_run_content_resource(
                json!({ "id": "docs:reviewer", "revision": entry["revision"] }),
                "references/guide.md".into(),
                skill_content_hash("guide"),
            )
            .await
            .expect("record");
        assert_eq!(
            store
                .list_run_content_resources(None)
                .await
                .expect("list")
                .len(),
            1
        );

        // Reopen: every managed table is durable through the file.
        drop(store);
        let reopened = SqliteStore::open(&path, 16).expect("reopen");
        assert_eq!(
            reopened.list_prompt_templates().await.expect("list").len(),
            2
        );
        let connection = reopened
            .get_connection("c1".into())
            .await
            .expect("get")
            .expect("present");
        assert_eq!(connection["revision"], json!(2));
        assert_eq!(
            connection["config"]["transport"]["env"]["TOKEN"],
            json!("***")
        );
        let skills = reopened.list_skills().await.expect("skills");
        assert_eq!(skills[0]["id"], json!("docs:reviewer"));
        // The on-disk record keeps the real env value; only reads redact.
        assert_eq!(
            store_records_of(&reopened).await,
            vec![skill_content_hash("guide")]
        );
        drop(reopened);
        let raw = rusqlite::Connection::open(&path).expect("raw open");
        let stored: String = raw
            .query_row(
                "SELECT record FROM connector_connections WHERE id = 'c1'",
                [],
                |row| row.get(0),
            )
            .expect("stored connection");
        assert!(
            stored.contains("secret"),
            "durable record keeps the literal"
        );
        assert!(
            !stored.contains("***"),
            "durable record is not the redaction"
        );
    }

    async fn store_records_of(store: &SqliteStore) -> Vec<String> {
        store
            .list_run_content_resources(Some("docs:reviewer".into()))
            .await
            .expect("resources")
            .into_iter()
            .map(|record| record["revision"].as_str().expect("revision").to_owned())
            .collect()
    }
}
