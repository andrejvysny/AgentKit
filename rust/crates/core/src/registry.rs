use crate::{
    Tool, ToolDefinition,
    call_pipeline::{AliasTable, Normalised, normalise},
    deferred::{Catalog, CatalogCell, CatalogEntry, TOOL_CALL, ToolLoading, is_meta_tool, namespace_of},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

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
/// The session's frozen tool surface (roadmap 1.3): the definitions sent
/// every turn, in manifest order, whatever the contributors answer now.
struct Frozen {
    surface: Vec<ToolDefinition>,
    /// Late tools: registered, but joined the deferred catalog and are not
    /// callable until a loader (tool_search, 2.3) promotes them.
    deferred: BTreeSet<String>,
}
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Entry>,
    order: Vec<String>,
    aliases: AliasTable,
    /// Known tools this turn may not use, with the reason shown to the model.
    withheld: BTreeMap<String, String>,
    /// Known tools a guard withheld at staging: offered in the definitions
    /// so the array does not change, refused when called.
    ghosts: BTreeMap<String, ToolDefinition>,
    frozen: OnceLock<Frozen>,
    /// Tools that ask to be deferred (their own [`ToolLoading`] hint).
    deferred_hints: BTreeSet<String>,
    /// The session catalog, filled at freeze and read by the meta tools.
    catalog: CatalogCell,
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
        for (alias, canonical) in tool.arg_aliases() {
            self.aliases.add_for_tool(name, &alias, &canonical);
        }
        if tool.loading() == ToolLoading::Deferred {
            self.deferred_hints.insert(name.clone());
        }
        self.order.push(name.clone());
        self.tools.insert(name.clone(), Entry { tool, validator });
        Ok(())
    }
    /// Register `tool_search`, `tool_describe` and `tool_call`, bound to this
    /// registry's catalog. Whether they reach the model is decided at freeze:
    /// a manifest that fits the budget leaves them out of the array.
    pub fn install_meta_tools(&mut self) -> Result<(), RegistryError> {
        for tool in crate::meta_tools::meta_tools(&self.catalog) {
            if !self.tools.contains_key(&tool.definition().name) {
                self.register(tool)?;
            }
        }
        Ok(())
    }
    /// Names of registered tools that ask to be deferred.
    pub fn deferred_hints(&self) -> BTreeSet<String> {
        self.deferred_hints.clone()
    }
    /// Fill the catalog from the frozen surface (direct entries) and the
    /// deferred definitions. The first call wins; false when already filled.
    pub fn freeze_catalog(&self, deferred: &[ToolDefinition]) -> bool {
        let surface = self
            .frozen
            .get()
            .map(|frozen| frozen.surface.as_slice())
            .unwrap_or_default();
        let meta_active = surface.iter().any(|definition| definition.name == TOOL_CALL);
        let entry = |definition: &ToolDefinition, deferred: bool| CatalogEntry {
            namespace: namespace_of(definition),
            available: self.tools.contains_key(&definition.name)
                && !self.withheld.contains_key(&definition.name),
            definition: definition.clone(),
            deferred,
        };
        let entries = surface
            .iter()
            .filter(|definition| !is_meta_tool(&definition.name))
            .map(|definition| entry(definition, false))
            .chain(deferred.iter().map(|definition| entry(definition, true)))
            .collect();
        self.catalog.set(Catalog::new(entries, meta_active)).is_ok()
    }
    /// True when this session's tools array carries the meta tools, so a
    /// deferred tool is called through `tool_call`.
    pub fn meta_active(&self) -> bool {
        self.catalog.get().is_some_and(Catalog::meta_active)
    }
    /// The session catalog, once the surface is frozen.
    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.get()
    }
    /// Parameter aliases for the call pipeline; per-tool entries go here.
    pub fn aliases_mut(&mut self) -> &mut AliasTable {
        &mut self.aliases
    }
    /// Record a known tool that is not offered this turn, so a call to it
    /// is refused with that reason and the nearest alternatives.
    pub fn withhold(&mut self, name: &str, reason: &str) {
        self.withheld.insert(name.into(), reason.into());
    }
    /// Keep `definition` in the offered tools but never run it: a guard
    /// decides at call time, so the array does not shrink (CE-05). A call
    /// is refused with `reason`. No effect when the name is registered.
    pub fn withhold_tool(&mut self, definition: ToolDefinition, reason: &str) {
        if self.tools.contains_key(&definition.name) {
            return;
        }
        self.withheld.insert(definition.name.clone(), reason.into());
        self.ghosts.insert(definition.name.clone(), definition);
    }
    /// Withheld-but-offered definitions with their reasons, name order.
    pub fn withheld_definitions(&self) -> Vec<(ToolDefinition, String)> {
        self.ghosts
            .iter()
            .map(|(name, definition)| {
                let reason = self.withheld.get(name).cloned().unwrap_or_default();
                (definition.clone(), reason)
            })
            .collect()
    }
    /// Freeze the offered definitions to `surface` (the session manifest, in
    /// manifest order) and mark `deferred` tools as not yet callable. The
    /// first freeze wins; returns false when already frozen.
    pub fn freeze_surface(&self, surface: Vec<ToolDefinition>, deferred: Vec<String>) -> bool {
        self.frozen
            .set(Frozen {
                surface,
                deferred: deferred.into_iter().collect(),
            })
            .is_ok()
    }
    /// True for a tool in the frozen surface that nothing can run this turn
    /// (its contributor is gone, slow or refused it).
    pub(crate) fn is_unavailable(&self, name: &str) -> bool {
        self.frozen.get().is_some_and(|frozen| {
            frozen
                .surface
                .iter()
                .any(|definition| definition.name == name)
                && !self.tools.contains_key(name)
        })
    }
    pub(crate) fn is_deferred(&self, name: &str) -> bool {
        self.frozen
            .get()
            .is_some_and(|frozen| frozen.deferred.contains(name))
    }
    pub(crate) fn withheld_reason(&self, name: &str) -> Option<&str> {
        self.withheld.get(name).map(String::as_str)
    }
    pub fn names(&self) -> Vec<String> {
        self.order.clone()
    }
    /// Normalise `args` for tool `name` (aliases, then types). Unknown tools
    /// pass through unchanged.
    pub fn normalise(&self, name: &str, args: Value) -> Normalised {
        match self.tools.get(name) {
            Some(entry) => normalise(
                &self.aliases,
                name,
                &entry.tool.definition().input_schema,
                args,
            ),
            None => Normalised {
                args,
                aliases_used: vec![],
                values_converted: 0,
            },
        }
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
    /// The tools offered to the model: the frozen manifest when the session
    /// has one, otherwise registered tools in order and then withheld ones.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        if let Some(frozen) = self.frozen.get() {
            return frozen.surface.clone();
        }
        self.order
            .iter()
            .map(|name| self.tools[name].tool.definition().clone())
            .chain(self.ghosts.values().cloned())
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
        output.insert(key.clone(), prepare_schema(child, name, depth + 1, nodes)?);
    }
    Ok(Value::Object(output))
}
