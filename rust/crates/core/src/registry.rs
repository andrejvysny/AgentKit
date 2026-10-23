use crate::{Tool, ToolDefinition};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct RegistryError {
    pub code: &'static str,
    pub tool_name: String,
    pub message: String,
}
struct Entry {
    tool: Arc<dyn Tool>,
    validator: agentkit_contracts::SchemaValidator,
}
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Entry>,
    order: Vec<String>,
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let name = &tool.definition().name;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err(error(
                "invalid_tool_name",
                name,
                &format!(
                    "Invalid tool name \"{name}\": must match ^[a-zA-Z0-9_-]+$ (no dots or spaces)."
                ),
            ));
        }
        if self.tools.contains_key(name) {
            return Err(error(
                "duplicate_tool",
                name,
                &format!("Duplicate tool registration: \"{name}\""),
            ));
        }
        let schema = prepare_schema(&tool.definition().input_schema, name, 1, &mut 0)?;
        let validator = agentkit_contracts::compile_tool_schema(&schema).map_err(|cause| {
            error(
                "schema_invalid",
                name,
                &format!("Invalid inputSchema for tool \"{name}\": {cause}"),
            )
        })?;
        self.order.push(name.clone());
        self.tools.insert(name.clone(), Entry { tool, validator });
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(|entry| entry.tool.clone())
    }
    pub fn validate_input(&self, name: &str, args: &Value) -> Vec<String> {
        self.tools
            .get(name)
            .map(|entry| {
                entry
                    .validator
                    .iter_errors(args)
                    .map(|err| format!("{}: {err}", err.instance_path))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.order
            .iter()
            .map(|name| self.tools[name].tool.definition().clone())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
fn error(code: &'static str, name: &str, message: &str) -> RegistryError {
    RegistryError {
        code,
        tool_name: name.into(),
        message: message.into(),
    }
}
fn prepare_schema(
    value: &Value,
    name: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<Value, RegistryError> {
    if !value.is_object() && !value.is_array() {
        return Ok(value.clone());
    }
    *nodes += 1;
    if depth > 32 || *nodes > 2000 {
        return Err(error(
            "schema_too_large",
            name,
            "Input schema exceeds 32 levels or 2000 nodes.",
        ));
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(|item| prepare_schema(item, name, depth + 1, nodes))
            .collect();
    }
    let mut output = Map::new();
    for (key, child) in value.as_object().expect("object checked") {
        if key != "$id" && key != "$schema" {
            output.insert(key.clone(), prepare_schema(child, name, depth + 1, nodes)?);
        }
    }
    Ok(Value::Object(output))
}
