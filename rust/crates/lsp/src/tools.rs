//! The six read-only AgentKit tools over [`LspPool`]. Positions are
//! 1-based in the tool interface and converted to LSP's 0-based form at
//! the boundary; results come back workspace-relative.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agentkit_core::{CancellationToken, CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::documents::resolve_confined;
use crate::error::{INVALID_INPUT, SERVER_NOT_FOUND, err};
use crate::pool::LspPool;
use crate::server::ServerHandle;
use crate::uri::uri_to_path;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    Definition,
    References,
    Hover,
    Symbols,
    Implementation,
    Diagnostics,
}

/// One LSP tool bound to a pool + workspace root.
pub struct LspTool {
    definition: ToolDefinition,
    kind: ToolKind,
    pool: Arc<LspPool>,
    root: PathBuf,
}

/// The six LSP tools for one workspace root. The language for positional
/// tools is chosen per file by extension; `language` overrides explicitly
/// on every tool.
pub fn lsp_tools(pool: Arc<LspPool>, workspace_root: &Path) -> Vec<Arc<dyn Tool>> {
    let root = workspace_root.to_path_buf();
    [
        (
            ToolKind::Definition,
            "lsp_definition",
            "code.navigation",
            "Find the definition(s) of the symbol at a 1-based file position (line/character).",
        ),
        (
            ToolKind::References,
            "lsp_references",
            "code.navigation",
            "Find all references of the symbol at a 1-based file position.",
        ),
        (
            ToolKind::Hover,
            "lsp_hover",
            "code.navigation",
            "Get hover documentation (type signature, docs) for the symbol at a 1-based file position.",
        ),
        (
            ToolKind::Symbols,
            "lsp_symbols",
            "code.navigation",
            "Search workspace symbols by fuzzy query.",
        ),
        (
            ToolKind::Implementation,
            "lsp_implementation",
            "code.navigation",
            "Find implementation(s) of the interface/trait method at a 1-based file position.",
        ),
        (
            ToolKind::Diagnostics,
            "lsp_diagnostics",
            "code.diagnostics",
            "Get normalized diagnostics (errors/warnings) for a file, or all documents opened so far when no file is given.",
        ),
    ]
    .into_iter()
    .map(|(kind, name, capability, description)| {
        Arc::new(LspTool {
            definition: ToolDefinition {
                name: name.to_owned(),
                version: "1.0.0".into(),
                effect: "read".into(),
                capability: capability.into(),
                description: description.into(),
                input_schema: input_schema(kind),
                output_schema: None,
                timeout_ms: None,
            },
            kind,
            pool: Arc::clone(&pool),
            root: root.clone(),
        }) as Arc<dyn Tool>
    })
    .collect()
}

impl LspTool {
    fn effect_context(&self) -> Value {
        json!({"workspaceRoot": self.root.to_string_lossy()})
    }
}

fn position_schema() -> Value {
    json!({"type": "number", "minimum": 1, "description": "1-based position"})
}

fn base_properties(extra: Vec<(&str, Value, bool)>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in extra {
        properties.insert(name.into(), schema);
        if is_required {
            required.push(json!(name));
        }
    }
    properties.insert(
        "language".into(),
        json!({"type": "string", "enum": ["rust", "typescript"],
               "description": "Override the file-extension language detection."}),
    );
    json!({"type": "object", "properties": properties, "required": required,
           "additionalProperties": false})
}

fn input_schema(kind: ToolKind) -> Value {
    match kind {
        ToolKind::Definition => base_properties(vec![
            ("file", json!({"type": "string"}), true),
            ("line", position_schema(), true),
            ("character", position_schema(), true),
        ]),
        ToolKind::References => base_properties(vec![
            ("file", json!({"type": "string"}), true),
            ("line", position_schema(), true),
            ("character", position_schema(), true),
            ("includeDeclaration", json!({"type": "boolean"}), false),
        ]),
        ToolKind::Hover => base_properties(vec![
            ("file", json!({"type": "string"}), true),
            ("line", position_schema(), true),
            ("character", position_schema(), true),
        ]),
        ToolKind::Implementation => base_properties(vec![
            ("file", json!({"type": "string"}), true),
            ("line", position_schema(), true),
            ("character", position_schema(), true),
        ]),
        ToolKind::Symbols => base_properties(vec![("query", json!({"type": "string"}), true)]),
        ToolKind::Diagnostics => base_properties(vec![(
            "file",
            json!({"type": "string",
                   "description": "Omit to collect diagnostics from all documents opened so far."}),
            false,
        )]),
    }
}
#[async_trait]
impl Tool for LspTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn effect_context(&self) -> Option<Value> {
        Some(self.effect_context())
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let args = input.as_object().cloned().unwrap_or_default();
        let language_override = match args.get("language") {
            None | Some(Value::Null) => None,
            Some(Value::String(lang)) => Some(lang.clone()),
            Some(other) => {
                return Err(err(
                    INVALID_INPUT,
                    format!("language must be a string, got {other}"),
                ));
            }
        };
        let canonical_root = LspPool::canonical_root(&self.root)?;
        let cancellation = context.cancellation.clone();
        let name = self.definition.name.clone();
        let handle = match self.kind {
            ToolKind::Symbols | ToolKind::Diagnostics => {
                let file = string_arg(&args, "file")?;
                let language = match &language_override {
                    Some(lang) => lang.clone(),
                    None => self.resolve_language(&canonical_root, file.as_deref())?,
                };
                Arc::clone(
                    &self
                        .pool
                        .acquire(&canonical_root, &language, &cancellation)
                        .await?,
                )
            }
            _ => {
                let raw = req_string(&args, "file")?;
                let path = resolve_confined(&canonical_root, &raw)?;
                let language = match &language_override {
                    Some(lang) => lang.clone(),
                    None => crate::discovery::language_for_path(&path)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            err(
                                SERVER_NOT_FOUND,
                                format!(
                                    "no language server is registered for the extension of {raw}; supported: rust (.rs), typescript (.ts/.tsx/.js/.jsx/.mts/.cts)",
                                ),
                            )
                        })?,
                };
                Arc::clone(
                    &self
                        .pool
                        .acquire(&canonical_root, &language, &cancellation)
                        .await?,
                )
            }
        };
        let payload = self
            .run(&handle, &args, &canonical_root, &cancellation)
            .await?;
        Ok(self.result(context, name, payload))
    }
}

impl LspTool {
    fn resolve_language(&self, root: &Path, file: Option<&str>) -> Result<String, CoreError> {
        if let Some(raw) = file {
            let path = resolve_confined(root, raw)?;
            return crate::discovery::language_for_path(&path)
                .map(str::to_owned)
                .ok_or_else(|| {
                    err(
                        SERVER_NOT_FOUND,
                        format!("no language server is registered for {}", path.display()),
                    )
                });
        }
        crate::discovery::detect_language(root)
            .map(str::to_owned)
            .ok_or_else(|| {
                err(
                    SERVER_NOT_FOUND,
                    format!(
                        "no supported source files (rust/typescript) found under {}",
                        root.display()
                    ),
                )
            })
    }

    async fn run(
        &self,
        handle: &Arc<ServerHandle>,
        args: &serde_json::Map<String, Value>,
        canonical_root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Value, CoreError> {
        let position_params = |uri: &str| -> Value {
            json!({
                "textDocument": {"uri": uri},
                "position": {
                    "line": position_arg(args, "line").unwrap_or(1) - 1,
                    "character": position_arg(args, "character").unwrap_or(1) - 1
                }
            })
        };
        match self.kind {
            ToolKind::Definition => {
                let path = resolve_confined(canonical_root, &req_string(args, "file")?)?;
                let result = handle
                    .guarded_request(
                        "textDocument/definition",
                        Some(&path),
                        &position_params,
                        cancellation,
                    )
                    .await?;
                Ok(json!({"definitions": self.locations(&result, canonical_root)}))
            }
            ToolKind::Implementation => {
                let path = resolve_confined(canonical_root, &req_string(args, "file")?)?;
                let result = handle
                    .guarded_request(
                        "textDocument/implementation",
                        Some(&path),
                        &position_params,
                        cancellation,
                    )
                    .await?;
                Ok(json!({"implementations": self.locations(&result, canonical_root)}))
            }
            ToolKind::References => {
                let path = resolve_confined(canonical_root, &req_string(args, "file")?)?;
                let include_declaration = args
                    .get("includeDeclaration")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let result = handle
                    .guarded_request(
                        "textDocument/references",
                        Some(&path),
                        &|uri: &str| {
                            let mut params = position_params(uri).as_object().unwrap().clone();
                            params.insert(
                                "context".into(),
                                json!({"includeDeclaration": include_declaration}),
                            );
                            Value::Object(params)
                        },
                        cancellation,
                    )
                    .await?;
                Ok(json!({"references": self.locations(&result, canonical_root)}))
            }
            ToolKind::Hover => {
                let path = resolve_confined(canonical_root, &req_string(args, "file")?)?;
                let result = handle
                    .guarded_request(
                        "textDocument/hover",
                        Some(&path),
                        &position_params,
                        cancellation,
                    )
                    .await?;
                let contents = flatten_hover(&result);
                Ok(json!({"contents": contents}))
            }
            ToolKind::Symbols => {
                let query = req_string(args, "query")?;
                let result = handle
                    .guarded_request(
                        "workspace/symbol",
                        None,
                        &|_uri: &str| json!({"query": query}),
                        cancellation,
                    )
                    .await?;
                Ok(json!({"symbols": self.symbols(&result, canonical_root)}))
            }
            ToolKind::Diagnostics => {
                let file = string_arg(args, "file")?;
                let items = match &file {
                    Some(raw) => {
                        let path = resolve_confined(canonical_root, raw)?;
                        handle.diagnostics_for_file(&path, cancellation).await?
                    }
                    None => handle.diagnostics_all_opened(cancellation).await?,
                };
                let items = items
                    .into_iter()
                    .map(|mut item| {
                        if let Some(Value::String(path)) = item.get("file").cloned()
                            && let Some(entry) = item.get_mut("file")
                        {
                            *entry = json!(strip_root(&path, canonical_root));
                        }
                        item
                    })
                    .collect::<Vec<_>>();
                Ok(json!({"diagnostics": items}))
            }
        }
    }
}
impl LspTool {
    /// `Location[] | LocationLink[] | Location | null` → tool shape
    /// (workspace-relative files, 1-based positions).
    fn locations(&self, result: &Value, root: &Path) -> Vec<Value> {
        let entries: Vec<&Value> = match result {
            Value::Array(items) => items.iter().collect(),
            Value::Null => vec![],
            single => vec![single],
        };
        entries
            .iter()
            .filter_map(|entry| {
                let (uri, range) = if entry.get("targetUri").is_some() {
                    (
                        entry.get("targetUri")?.as_str()?.to_owned(),
                        entry.get("targetSelectionRange").cloned()?,
                    )
                } else {
                    (
                        entry.get("uri")?.as_str()?.to_owned(),
                        entry.get("range").cloned()?,
                    )
                };
                let mut out = serde_json::Map::new();
                out.insert("file".into(), json!(self.relative_file(&uri, root)));
                insert_position(&mut out, "line", "character", &range, "start");
                insert_position(&mut out, "endLine", "endCharacter", &range, "end");
                Some(Value::Object(out))
            })
            .collect()
    }

    fn symbols(&self, result: &Value, root: &Path) -> Vec<Value> {
        let entries: Vec<&Value> = result
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        entries
            .iter()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.to_owned();
                let kind = entry
                    .get("kind")
                    .and_then(Value::as_i64)
                    .map(symbol_kind_name)
                    .unwrap_or_else(|| "unknown");
                let location = entry.get("location")?;
                let range = if location.get("range").is_some() {
                    location.get("range")?.clone()
                } else {
                    location.get("selectionRange")?.clone()
                };
                let uri = location.get("uri")?.as_str()?.to_owned();
                let mut out = serde_json::Map::new();
                out.insert("name".into(), json!(name));
                out.insert("kind".into(), json!(kind));
                out.insert("file".into(), json!(self.relative_file(&uri, root)));
                insert_position(&mut out, "line", "character", &range, "start");
                Some(Value::Object(out))
            })
            .collect()
    }

    fn relative_file(&self, uri: &str, root: &Path) -> String {
        match uri_to_path(uri) {
            Ok(path) => path
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned()),
            Err(_) => uri.to_owned(),
        }
    }

    /// Wrap a payload into the AgentKit result envelope, honouring
    /// [`agentkit_core::ToolLimits`] (item count + byte budget) on the
    /// single top-level list this crate produces.
    fn result(&self, context: ToolContext, name: String, mut payload: Value) -> ToolResult {
        let mut truncated = false;
        const LISTS: [&str; 5] = [
            "definitions",
            "references",
            "implementations",
            "symbols",
            "diagnostics",
        ];
        if let Some(key) = LISTS.iter().copied().find(|k| payload.get(*k).is_some()) {
            if let Some(max_items) = context.limits.max_items
                && let Some(list) = payload.get_mut(key).and_then(Value::as_array_mut)
                && list.len() > max_items
            {
                list.truncate(max_items);
                truncated = true;
            }
            loop {
                let too_big = serde_json::to_string(&payload)
                    .map(|serialized| serialized.len())
                    .unwrap_or(0)
                    > context.limits.max_bytes;
                let non_empty = payload
                    .get(key)
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty());
                if !too_big || !non_empty {
                    break;
                }
                if let Some(list) = payload.get_mut(key).and_then(Value::as_array_mut) {
                    list.pop();
                }
                truncated = true;
            }
        }
        let count = payload
            .as_object()
            .and_then(|map| map.values().find_map(Value::as_array))
            .map_or(0, Vec::len);
        let summary = match self.kind {
            ToolKind::Hover => "hover information".to_owned(),
            _ => format!("{count} result(s)"),
        };
        let mut warnings = Vec::new();
        if truncated {
            warnings.push(format!(
                "{name} truncated results to fit the tool limits ({} bytes, {:?} items).",
                context.limits.max_bytes, context.limits.max_items
            ));
        }
        let mut sources = Vec::new();
        if let Some(file) = payload.get("file").and_then(Value::as_str) {
            sources.push(json!({"kind": "file", "path": file}));
        }
        ToolResult {
            ok: true,
            data: Some(payload.clone()),
            sources,
            warnings,
            truncated,
            limits: context.limits.clone(),
            model_data: Some(payload),
            summary: Some(format!("{}: {summary}", self.definition.name)),
            status: Some("ok".into()),
            model_content: None,
        }
    }
}

/// Workspace-relative display path (absolute when outside the root).
fn strip_root(path: &str, root: &Path) -> String {
    Path::new(path)
        .strip_prefix(root)
        .map(|rel| rel.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

fn insert_position(
    out: &mut serde_json::Map<String, Value>,
    line_key: &str,
    char_key: &str,
    range: &Value,
    side: &str,
) {
    if let Some(point) = range.get(side) {
        out.insert(
            line_key.into(),
            json!(point.get("line").and_then(Value::as_i64).unwrap_or(0) + 1),
        );
        out.insert(
            char_key.into(),
            json!(point.get("character").and_then(Value::as_i64).unwrap_or(0) + 1),
        );
    }
}

/// LSP SymbolKind number → readable name.
fn symbol_kind_name(kind: i64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enumMember",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "typeParameter",
        _ => "unknown",
    }
}

/// `Hover.contents` (MarkupContent | MarkedString | MarkedString[]) →
/// one markdown-ish string.
fn flatten_hover(hover: &Value) -> String {
    let contents = match hover.get("contents") {
        None | Some(Value::Null) => return String::new(),
        Some(value) => value,
    };
    fn one(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Object(map) => {
                if let Some(text) = map.get("value").and_then(Value::as_str) {
                    match map.get("language").and_then(Value::as_str) {
                        Some(lang) if !lang.is_empty() => format!("```{lang}\n{text}\n```"),
                        _ => text.to_owned(),
                    }
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        }
    }
    match contents {
        Value::Array(parts) => parts
            .iter()
            .map(one)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => one(other),
    }
}

fn req_string(args: &serde_json::Map<String, Value>, key: &str) -> Result<String, CoreError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            err(
                INVALID_INPUT,
                format!("{key} is required and must be a non-empty string"),
            )
        })
}

fn string_arg(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, CoreError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(Value::String(_)) => Ok(None),
        Some(other) => Err(err(
            INVALID_INPUT,
            format!("{key} must be a string, got {other}"),
        )),
    }
}

fn position_arg(args: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|f| f.fract() == 0.0)
                .map(|f| f as i64)
        }),
        _ => None,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use crate::LspConfig;

    use super::*;

    #[test]
    fn symbol_kinds_map() {
        assert_eq!(symbol_kind_name(12), "function");
        assert_eq!(symbol_kind_name(23), "struct");
        assert_eq!(symbol_kind_name(99), "unknown");
    }

    #[test]
    fn hover_flattening_covers_all_shapes() {
        assert_eq!(
            flatten_hover(&json!({"contents":{"kind":"markdown","value":"**hi**"}})),
            "**hi**"
        );
        assert_eq!(flatten_hover(&json!({"contents":"plain"})), "plain");
        assert_eq!(
            flatten_hover(
                &json!({"contents":[{"language":"rust","value":"fn a()"},{"language":"","value":"tail"}]})
            ),
            "```rust\nfn a()\n```\n\ntail"
        );
        assert_eq!(flatten_hover(&json!({})), "");
        assert_eq!(flatten_hover(&json!({"contents":null})), "");
    }

    fn tool(kind: ToolKind) -> LspTool {
        LspTool {
            definition: ToolDefinition {
                name: "t".into(),
                version: "1".into(),
                effect: "read".into(),
                capability: "code.navigation".into(),
                description: String::new(),
                input_schema: json!({}),
                output_schema: None,
                timeout_ms: None,
            },
            kind,
            pool: LspPool::new(LspConfig::default()),
            root: PathBuf::from("/ws"),
        }
    }

    #[test]
    fn locations_normalize_both_shapes() {
        let t = tool(ToolKind::Definition);
        let root = Path::new("/ws");
        let out = t.locations(
            &json!([
                {"uri":"file:///ws/src/a.rs","range":{"start":{"line":9,"character":4},
                 "end":{"line":9,"character":10}}},
                {"targetUri":"file:///ws/src/b.rs","targetSelectionRange":
                 {"start":{"line":1,"character":0},"end":{"line":1,"character":3}}},
                {"uri":"file:///elsewhere/x.ts","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                {"junk":true}
            ]),
            root,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["file"], "src/a.rs");
        assert_eq!(out[0]["line"], 10);
        assert_eq!(out[0]["character"], 5);
        assert_eq!(out[0]["endLine"], 10);
        assert_eq!(out[0]["endCharacter"], 11);
        assert_eq!(out[1]["file"], "src/b.rs");
        assert_eq!(out[2]["file"], "/elsewhere/x.ts");
    }

    #[test]
    fn symbols_normalize() {
        let t = tool(ToolKind::Symbols);
        let out = t.symbols(
            &json!([
                {"name":"target","kind":12,
                 "location":{"uri":"file:///ws/src/lib.rs",
                             "range":{"start":{"line":2,"character":7},"end":{"line":2,"character":13}}}},
                {"name":"bad"}
            ]),
            Path::new("/ws"),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "target");
        assert_eq!(out[0]["kind"], "function");
        assert_eq!(out[0]["file"], "src/lib.rs");
        assert_eq!(out[0]["line"], 3);
        assert_eq!(out[0]["character"], 8);
    }

    #[test]
    fn limits_truncate_lists() {
        let t = tool(ToolKind::References);
        let context = ToolContext {
            limits: agentkit_core::ToolLimits {
                profile: "small".into(),
                max_bytes: 16384,
                max_items: Some(2),
            },
            ..test_context()
        };
        let result = t.result(
            context,
            "lsp_references".into(),
            json!({"references":[{"file":"a"},{"file":"b"},{"file":"c"}]}),
        );
        assert!(result.truncated);
        assert_eq!(
            result.data.as_ref().unwrap()["references"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(result.model_data, result.data);
        assert!(!result.warnings.is_empty());
        assert_eq!(result.limits.max_items, Some(2));
    }

    #[test]
    fn byte_budget_drops_items() {
        let t = tool(ToolKind::Symbols);
        let context = ToolContext {
            limits: agentkit_core::ToolLimits {
                profile: "tiny".into(),
                max_bytes: 80,
                max_items: Some(50),
            },
            ..test_context()
        };
        let result = t.result(
            context,
            "lsp_symbols".into(),
            json!({"symbols":[{"name":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},{"name":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},{"name":"c"}]}),
        );
        assert!(result.truncated);
        assert_eq!(
            result.data.as_ref().unwrap()["symbols"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    fn test_context() -> ToolContext {
        ToolContext {
            run_id: "r".into(),
            tool_call_id: None,
            attempt_id: None,
            chat_id: None,
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: agentkit_core::ToolLimits::default(),
            cancellation: CancellationToken::new(),
            dispatch_guard: None,
            metadata: serde_json::Map::new(),
            suspension: None,
        }
    }

    #[test]
    fn position_args_accept_integral_floats() {
        let args = json!({"line": 3.0, "character": 7})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(position_arg(&args, "line"), Some(3));
        assert_eq!(position_arg(&args, "character"), Some(7));
        assert_eq!(position_arg(&args, "missing"), None);
    }

    #[test]
    fn tool_names_and_capabilities() {
        let tools = lsp_tools(LspPool::new(LspConfig::default()), Path::new("/ws"));
        let defs: Vec<_> = tools.iter().map(|t| t.definition()).collect();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "lsp_definition",
                "lsp_references",
                "lsp_hover",
                "lsp_symbols",
                "lsp_implementation",
                "lsp_diagnostics"
            ]
        );
        for def in &defs {
            assert_eq!(def.effect, "read");
            assert_eq!(def.version, "1.0.0");
        }
        assert_eq!(defs[5].capability, "code.diagnostics");
        assert_eq!(defs[0].capability, "code.navigation");
        // effect context carries the workspace root.
        assert_eq!(tools[0].effect_context().unwrap()["workspaceRoot"], "/ws");
    }
}
