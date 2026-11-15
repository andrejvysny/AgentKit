use crate::{
    CoreError, Tool, ToolDefinition,
    call_pipeline::{AliasTable, Normalised, normalise},
    dialect::{Dialect, DialectError, Translated},
    deferred::{
        Catalog, CatalogCell, CatalogEntry, LoadMode, TOOL_CALL, ToolLoading, is_meta_tool,
        namespace_of,
    },
    toolset::{
        META_TOOLSET, Promotion, PromotionCell, PromotionObserver, Toolset, ToolsetView,
        meta_toolset,
    },
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
    /// How the surface is presented to the model (T3.1). Names in `surface`
    /// and `deferred` are the dialect's wire names; everything else in the
    /// registry is keyed by canonical id. `None` for a session whose list
    /// was frozen before dialects existed.
    dialect: Option<Dialect>,
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
    /// Registered toolsets by id (T4.1), and the toolset each tool is
    /// listed in or declares for itself, by canonical id.
    toolsets: BTreeMap<String, Toolset>,
    listed: BTreeMap<String, String>,
    declared: BTreeMap<String, String>,
    /// Toolsets `toolset_load` asked to promote, and the ones that joined.
    promotions: PromotionCell,
    /// The host's record of a promotion (re-stamps the session manifest).
    observer: OnceLock<Arc<dyn PromotionObserver>>,
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
        if let Some(toolset) = tool.toolset() {
            self.declared.insert(name.clone(), toolset.to_owned());
        }
        self.order.push(name.clone());
        self.tools.insert(name.clone(), Entry { tool, validator });
        Ok(())
    }
    /// Register `tool_search`, `tool_describe` and `tool_call`, bound to this
    /// registry's catalog. Whether they reach the model is decided at freeze:
    /// a manifest that fits the budget leaves them out of the array.
    pub fn install_meta_tools(&mut self) -> Result<(), RegistryError> {
        self.install_meta_tools_with(LoadMode::Indirect)
    }
    /// [`ToolRegistry::install_meta_tools`] with the session's load mode
    /// (T4.1): in promote mode `toolset_load` promotes the loaded tools.
    pub fn install_meta_tools_with(&mut self, mode: LoadMode) -> Result<(), RegistryError> {
        for tool in crate::meta_tools::meta_tools(&self.catalog, &self.promotions, mode) {
            if !self.tools.contains_key(&tool.definition().name) {
                self.register(tool)?;
            }
        }
        self.toolsets
            .entry(META_TOOLSET.to_owned())
            .or_insert_with(meta_toolset);
        Ok(())
    }
    /// Register a toolset (T4.1): its card, guide and member tools. The
    /// first registration of an id wins, so a host and a lane may both
    /// offer the same set.
    pub fn register_toolset(&mut self, toolset: Toolset) {
        for tool in &toolset.tools {
            self.listed
                .entry(tool.clone())
                .or_insert_with(|| toolset.id.clone());
        }
        self.toolsets.entry(toolset.id.clone()).or_insert(toolset);
    }
    /// The registered toolsets, id order.
    pub fn toolsets(&self) -> Vec<Toolset> {
        self.toolsets.values().cloned().collect()
    }
    /// The toolset of canonical tool `canonical`: the tool's own declaration,
    /// else the set that lists it, else the dynamic set its name implies
    /// (`mcp__alias`).
    fn toolset_id_for(&self, canonical: &str, definition: &ToolDefinition) -> String {
        self.declared
            .get(canonical)
            .or_else(|| self.listed.get(canonical))
            .cloned()
            .unwrap_or_else(|| namespace_of(definition))
    }
    /// The toolsets and the toolset of each of `offered` (names as the model
    /// sees them under `dialect`).
    pub fn toolset_view(
        &self,
        offered: &[ToolDefinition],
        dialect: Option<&Dialect>,
    ) -> ToolsetView {
        let mut toolsets = self.toolsets.clone();
        toolsets
            .entry(META_TOOLSET.to_owned())
            .or_insert_with(meta_toolset);
        let names = offered
            .iter()
            .map(|definition| {
                let canonical = dialect
                    .and_then(|dialect| dialect.canonical_of(&definition.name))
                    .unwrap_or(&definition.name);
                let id = if is_meta_tool(&definition.name) {
                    META_TOOLSET.to_owned()
                } else {
                    self.toolset_id_for(canonical, definition)
                };
                (definition.name.clone(), id)
            })
            .collect();
        ToolsetView::new(toolsets, names)
    }
    /// Let the host record promotions (re-stamp the session manifest). The
    /// first observer wins.
    pub fn set_promotion_observer(&self, observer: Arc<dyn PromotionObserver>) -> bool {
        self.observer.set(observer).is_ok()
    }
    /// The turn boundary of promote mode: toolsets that `toolset_load`
    /// asked for join the tools array now, never mid-turn. Each is recorded
    /// through the observer before the next request is built.
    pub async fn commit_promotions(&self) -> Result<Vec<Promotion>, CoreError> {
        let Some(catalog) = self.catalog.get() else {
            return Ok(vec![]);
        };
        let mut done = vec![];
        for toolset in self.promotions.take_pending() {
            let definitions: Vec<ToolDefinition> = catalog
                .entries()
                .iter()
                .filter(|entry| {
                    entry.deferred
                        && entry.available
                        && entry.namespace == toolset
                        && !self.promotions.is_promoted_name(&entry.definition.name)
                })
                .map(|entry| entry.definition.clone())
                .collect();
            if definitions.is_empty() {
                continue;
            }
            let revision = match self.observer.get() {
                Some(observer) => observer.promoted(&toolset, &definitions).await?,
                None => None,
            };
            let tools = definitions.iter().map(|d| d.name.clone()).collect();
            self.promotions.apply(&toolset, definitions);
            done.push(Promotion {
                toolset,
                tools,
                revision,
            });
        }
        Ok(done)
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
        let offered: Vec<ToolDefinition> = surface.iter().chain(deferred).cloned().collect();
        let view = self.toolset_view(&offered, self.dialect());
        let entry = |definition: &ToolDefinition, deferred: bool| {
            let canonical = self.canonical_name(&definition.name);
            CatalogEntry {
                namespace: view.id_of(definition),
                available: self.tools.contains_key(&canonical)
                    && !self.withheld.contains_key(&canonical),
                canonical,
                definition: definition.clone(),
                deferred,
            }
        };
        let entries = surface
            .iter()
            .filter(|definition| !is_meta_tool(&definition.name))
            .map(|definition| entry(definition, false))
            .chain(deferred.iter().map(|definition| entry(definition, true)))
            .collect();
        let catalog = Catalog::new(entries, meta_active).with_toolsets(view.clone());
        self.catalog.set(catalog).is_ok()
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
        self.freeze(surface, deferred, None)
    }
    /// [`ToolRegistry::freeze_surface`] for a surface rendered by `dialect`:
    /// calls are translated back to canonical before anything else runs.
    pub fn freeze_presented(
        &self,
        surface: Vec<ToolDefinition>,
        deferred: Vec<String>,
        dialect: Dialect,
    ) -> bool {
        self.freeze(surface, deferred, Some(dialect))
    }
    fn freeze(
        &self,
        surface: Vec<ToolDefinition>,
        deferred: Vec<String>,
        dialect: Option<Dialect>,
    ) -> bool {
        self.frozen
            .set(Frozen {
                surface,
                deferred: deferred.into_iter().collect(),
                dialect,
            })
            .is_ok()
    }
    /// The dialect the frozen surface is presented in, if any.
    pub fn dialect(&self) -> Option<&Dialect> {
        self.frozen.get().and_then(|frozen| frozen.dialect.as_ref())
    }
    /// The canonical call for a call emitted under the session's dialect:
    /// the one place a wire name becomes a canonical id. Identity without a
    /// dialect. Anything that runs a tool by name (the call pipeline, the
    /// inner call of `tool_call`) goes through it.
    pub fn translate(&self, name: &str, args: Value) -> Result<Translated, DialectError> {
        match self.dialect() {
            Some(dialect) => {
                let mut translated = dialect.translate(name, args)?;
                // Strict schemas make the model send null for an optional
                // argument; the canonical call leaves it out (T3.3).
                if dialect.strict()
                    && let Some(tool) = self.tools.get(&translated.name)
                    && crate::dialect::drop_nulls(
                        &mut translated.args,
                        &tool.tool.definition().input_schema,
                    )
                {
                    translated.args_changed = true;
                }
                Ok(translated)
            }
            None => Ok(Translated {
                name: name.to_owned(),
                args,
                args_changed: false,
            }),
        }
    }
    /// The canonical id for a name the model may use (a wire name of the
    /// session's dialect, or any other name unchanged).
    pub fn canonical_name(&self, name: &str) -> String {
        self.canonical_in_surface(name).to_owned()
    }
    /// The name the model sees for canonical tool `name`.
    pub fn present_name(&self, name: &str) -> String {
        self.dialect()
            .and_then(|dialect| dialect.wire_names_of(name).into_iter().next())
            .unwrap_or_else(|| name.to_owned())
    }
    /// Names of the registered tools as the model sees them.
    pub fn presented_names(&self) -> Vec<String> {
        let Some(dialect) = self.dialect() else {
            return self.names();
        };
        self.order
            .iter()
            .flat_map(|name| match dialect.wire_names_of(name) {
                wires if wires.is_empty() => vec![name.clone()],
                wires => wires,
            })
            .collect()
    }
    fn canonical_in_surface<'a>(&'a self, name: &'a str) -> &'a str {
        self.dialect()
            .and_then(|dialect| dialect.canonical_of(name))
            .unwrap_or(name)
    }
    /// True for a tool in the frozen surface that nothing can run this turn
    /// (its contributor is gone, slow or refused it).
    pub(crate) fn is_unavailable(&self, name: &str) -> bool {
        self.frozen.get().is_some_and(|frozen| {
            frozen
                .surface
                .iter()
                .any(|definition| self.canonical_in_surface(&definition.name) == name)
                && !self.tools.contains_key(name)
        })
    }
    pub(crate) fn is_deferred(&self, name: &str) -> bool {
        self.frozen.get().is_some_and(|frozen| {
            frozen.deferred.iter().any(|deferred| {
                self.canonical_in_surface(deferred) == name
                    && !self.promotions.is_promoted_name(deferred)
            })
        })
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
            let mut surface = frozen.surface.clone();
            let promoted = self.promotions.promoted_definitions();
            if !promoted.is_empty() {
                // Promoted tools join in manifest (name) order.
                surface.extend(promoted);
                surface.sort_by(|a, b| a.name.cmp(&b.name));
            }
            return surface;
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
