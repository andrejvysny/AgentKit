//! The MCP client side, hosted by the native store handle.
//!
//! `NativeStore` owns the crate's `McpClientManager`: each `mcpConnect`
//! adds one validated `ServerConfig` (stdio command/env/args OR url +
//! headers, exactly the crate's serde shape), connects it, and returns
//! descriptors the consumer registers through the EXISTING `registerTool`
//! lane — the effect on every descriptor comes SOLELY from the host's own
//! `readOnly` config (the crate's host-side allowance; remote annotations
//! are descriptive only, see `rust/crates/mcp/src/bridge.rs`), so the
//! `BindingAuthority` + `JsToolGuard` chain keeps full authority over
//! when a remote-backed tool may act. `mcpCallTool` is the host call used
//! from inside that lane: it re-validates the arguments against the
//! descriptor's recorded input schema LOCALLY (fail-closed: a schema the
//! contracts compiler rejects can validate nothing, so nothing is ever
//! dispatched unchecked — the same rule as the crate's `RemoteTool`)
//! before the crate's manager talks to the server. Resolved secret
//! material never appears here: the binding installs a resolver that
//! provides nothing, so any `secretRefs` entry fails its connect with
//! `mcp_secret_missing` rather than leaking or silently dropping a value.
//! Configs live in memory only, on this store handle; a process restart
//! re-declares them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use agentkit_core::{ToolLimits, ToolResult};
use agentkit_mcp::model::{CallToolResult, Tool as RemoteTool};
use agentkit_mcp::{
    McpClientError, McpClientManager, McpSecretResolver, ServerConfig, TransportConfig,
};
use async_trait::async_trait;
use napi::bindgen_prelude::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::NativeStore;
use crate::errors;

/// The store-side client state: the crate manager, the accepted configs
/// (declarations only — resolved secrets never enter this state), and the
/// per-alias projected descriptors with their pre-compiled validators.
pub(crate) struct Registry {
    /// Shared so `mcp_call_tool` can drop the store-wide lock before the
    /// network round-trip: the engine dispatches parallel tool calls, and
    /// serializing them behind a binding mutex would fake timeouts. The
    /// crate manager is internally safe for concurrent use (per-session
    /// lifecycles, serialized config swaps via its own update gate).
    manager: Arc<McpClientManager>,
    configs: BTreeMap<String, ServerConfig>,
    tools: HashMap<String, Vec<CachedTool>>,
}

/// One projected remote tool. Projection mirrors the crate's own
/// `McpToolSet::project` (`rust/crates/mcp/src/bridge.rs:120-160`) — same
/// canonical identity `mcp.<alias>.<effective>`, same `__`-flattened
/// descriptor name, effect from the HOST `readOnly` allowance only.
pub(crate) struct CachedTool {
    original: String,
    effective: String,
    canonical: String,
    descriptor: Value,
    validator: Option<agentkit_contracts::SchemaValidator>,
}

/// The binding's secret resolver: nothing is available in-process, so a
/// config carrying `secretRefs` fails closed at connect with
/// `mcp_secret_missing`. This is a documented gap (a host-provided
/// resolver will replace it), never a fake lookup.
struct UnavailableSecrets;

#[async_trait]
impl McpSecretResolver for UnavailableSecrets {
    async fn get(&self, _reference: &str) -> std::result::Result<Option<String>, McpClientError> {
        Ok(None)
    }
}

fn map_client(error: McpClientError) -> napi::Error {
    errors::stable(error.code(), error.to_string(), None)
}

/// Mirror of the crate's remote-tool identity rule
/// (`rust/crates/mcp/src/bridge.rs:291-299`): lowercase first byte,
/// dot-separated segments of lowercase ASCII, digits, `_` and `-`. A
/// remote tool that fails it is refused at connect, so a descriptor can
/// never carry an identity the registerTool lane would mis-execute.
fn tool_name_valid(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
        })
}

/// Project one listed remote tool into the host descriptor contract
/// (`ToolDefinitionJson` plus `remoteName`, the unmodified server-side
/// name `mcpCallTool` accepts). `version`/`capability`/`effect` follow the
/// crate's contributed-tool shape byte-for-byte.
fn project_tool(
    alias: &str,
    config: &ServerConfig,
    tool: RemoteTool,
) -> std::result::Result<CachedTool, McpClientError> {
    let original = tool.name.trim().to_owned();
    if !tool_name_valid(&original) {
        return Err(McpClientError::Configuration);
    }
    let effective = config
        .tool_aliases
        .get(&original)
        .cloned()
        .unwrap_or_else(|| original.clone());
    if !tool_name_valid(&effective) {
        return Err(McpClientError::Configuration);
    }
    let canonical = format!("mcp.{alias}.{effective}");
    let definition = json!({
        "name": canonical.replace('.', "__"),
        "version": "1.0.0",
        "effect": if config.read_only { "read" } else { "write" },
        "capability": canonical,
        "description": tool
            .description
            .map(|text| text.into_owned())
            .unwrap_or_else(|| format!("MCP tool {original} from {alias}")),
        "inputSchema": Value::Object((*tool.input_schema).clone()),
        "remoteName": original,
    });
    // Pre-compile the recorded schema so every call is gated locally
    // before a byte leaves the process; a rejected schema validates
    // nothing and the tool fails closed at call time instead.
    let schema = definition
        .get("inputSchema")
        .cloned()
        .unwrap_or(Value::Null);
    let validator = agentkit_contracts::compile_tool_schema(&schema).ok();
    Ok(CachedTool {
        original,
        effective,
        canonical,
        descriptor: definition,
        validator,
    })
}

/// The host tool-result contract for one remote call — the same shape a
/// JS tool settles with, so a `registerTool`-lane run function can return
/// it verbatim and the engine records it honestly. Mirrors the crate's
/// `RemoteTool::outcome` (`rust/crates/mcp/src/bridge.rs:205-262`):
/// concatenated text, non-text parts reported as a warning, `isError`
/// never laundered into success.
fn outcome(canonical: &str, alias: &str, original: &str, result: CallToolResult) -> Value {
    let content = serde_json::to_value(result.content).unwrap_or_else(|_| json!([]));
    let mut texts = Vec::new();
    let mut non_text: Vec<String> = Vec::new();
    for part in content.as_array().into_iter().flatten() {
        match (part["type"].as_str(), part["text"].as_str()) {
            (Some("text"), Some(text)) => texts.push(text.to_owned()),
            (Some(kind), _) if !non_text.iter().any(|entry| entry == kind) => {
                non_text.push((*kind).to_owned())
            }
            _ => {}
        }
    }
    let text = texts.join("\n");
    let is_error = result.is_error == Some(true);
    let mut data = json!({
        "canonicalId": canonical,
        "serverAlias": alias,
        "toolName": original,
        "text": text,
        "content": content,
        "nonTextTypes": non_text,
        "isError": is_error,
    });
    let structured = result.structured_content.filter(Value::is_object);
    if let Some(structured) = &structured {
        data["structuredContent"] = structured.clone();
    }
    let mut model = if is_error {
        json!({"errorCode":"mcp_tool_error","errorMessage":text,"retryable":false})
    } else {
        json!({"text":text})
    };
    if !is_error && let Some(structured) = &structured {
        model["structured"] = structured.clone();
    }
    let mut warnings = Vec::new();
    if !is_error && !non_text.is_empty() {
        warnings.push(format!(
            "MCP tool returned non-text content ({}); only text parts were passed to the model.",
            non_text.join(", ")
        ));
    }
    serde_json::to_value(ToolResult {
        ok: !is_error,
        data: Some(data),
        model_data: Some(model),
        summary: Some(if text.is_empty() {
            format!("{canonical} returned no text.")
        } else {
            text.lines().next().unwrap_or_default().to_owned()
        }),
        status: (!is_error).then(|| "ok".to_owned()),
        sources: Vec::new(),
        warnings,
        truncated: false,
        limits: ToolLimits::default(),
    })
    .unwrap_or_else(|_| json!({"ok":false,"data":{"errorCode":"internal_error"}}))
}

#[napi]
impl NativeStore {
    /// Connect one MCP server. `configJson` is the crate's `ServerConfig`
    /// shape: `{alias, transport: {kind:"stdio", command, args?, env?} |
    /// {kind:"http", url, headers?}, secretRefs?, enabled?, toolAliases?,
    /// readOnly?, resilience?}`. Effects are HOST-controlled: every
    /// returned descriptor carries `effect:"write"` UNLESS the host config
    /// sets `readOnly:true` — remote annotations never raise or lower it.
    /// Returns `{alias, tools}` where `tools` are descriptors ready for
    /// `NativeSessionService.registerTool`; call `mcpCallTool(alias,
    /// descriptor.remoteName, args)` from the tool lane to invoke. Fails
    /// with `mcp_invalid_config` for a bad declaration or remote identity,
    /// `mcp_connect_failed`/`mcp_*` for transport problems, and
    /// `mcp_secret_missing` if the config carries `secretRefs` (the
    /// binding has no secret resolver yet). Connecting an already-known
    /// alias fails with `invalid_input`; disconnect it first.
    #[napi]
    pub async fn mcp_connect(&self, config_json: String) -> Result<String> {
        let config: ServerConfig = serde_json::from_str(&config_json).map_err(|error| {
            errors::invalid(format!(
                "configJson must be a valid MCP server configuration: {error}"
            ))
        })?;
        if !config.enabled {
            return Err(errors::invalid(
                "mcpConnect never connects a disabled server; omit enabled or set it true",
            ));
        }
        let mut state = self.mcp.lock().await;
        if state.is_none() {
            let manager = McpClientManager::new(Vec::new(), Arc::new(UnavailableSecrets))
                .map_err(map_client)?;
            *state = Some(Registry {
                manager: Arc::new(manager),
                configs: BTreeMap::new(),
                tools: HashMap::new(),
            });
        }
        let registry = state
            .as_mut()
            .ok_or_else(|| errors::internal("MCP registry state was not installed"))?;
        if registry.configs.contains_key(&config.alias) {
            return Err(errors::invalid(format!(
                "MCP server {} is already connected; call mcpDisconnect() first",
                config.alias
            )));
        }
        let alias = config.alias.clone();
        let previous = registry.configs.clone();
        let mut configs = previous.clone();
        configs.insert(alias.clone(), config.clone());
        let taken_tools = std::mem::take(&mut registry.tools);
        registry
            .manager
            .replace_configs(configs.values().cloned().collect())
            .await
            .map_err(map_client)?;
        let cancellation = CancellationToken::new();
        let projected = async {
            registry
                .manager
                .connect(&alias, cancellation.clone())
                .await?;
            let listed = registry.manager.list_tools(&alias, cancellation).await?;
            let mut projected = Vec::new();
            let mut names: Vec<String> = taken_tools
                .values()
                .flatten()
                .filter_map(|entry| entry.descriptor.get("name")?.as_str().map(str::to_owned))
                .collect();
            let mut canonicals: Vec<String> = taken_tools
                .values()
                .flatten()
                .map(|entry| entry.canonical.clone())
                .collect();
            for tool in listed {
                let entry = project_tool(&alias, &config, tool)?;
                let name = entry
                    .descriptor
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if names.iter().any(|seen| seen == &name)
                    || canonicals.iter().any(|seen| seen == &entry.canonical)
                {
                    return Err(McpClientError::Configuration);
                }
                names.push(name);
                canonicals.push(entry.canonical.clone());
                projected.push(entry);
            }
            Ok::<_, McpClientError>(projected)
        }
        .await;
        let projected = match projected {
            Ok(projected) => projected,
            Err(error) => {
                // All-or-nothing: a failed connect leaves no half-known
                // server behind — roll the declaration back and close any
                // transport the attempt opened.
                let rollback = previous.values().cloned().collect::<Vec<_>>();
                registry.configs = previous;
                registry.tools = taken_tools;
                let _ = registry.manager.replace_configs(rollback).await;
                return Err(map_client(error));
            }
        };
        registry.configs = configs;
        registry.tools = taken_tools;
        registry.tools.insert(alias.clone(), projected);
        Ok(json!({
            "alias": alias,
            "tools": registry
                .tools
                .get(&alias)
                .map(|entries| entries.iter().map(|entry| entry.descriptor.clone()).collect::<Vec<_>>())
                .unwrap_or_default(),
        })
        .to_string())
    }

    /// Invoke one tool on a connected MCP server. `toolName` accepts the
    /// remote's original name, the configured alias target, or the full
    /// descriptor name. The arguments are validated LOCALLY against the
    /// descriptor's recorded input schema before dispatch (invalid
    /// arguments never reach the server, `mcp_invalid_arguments`; a
    /// schema that could not be compiled fails closed the same way).
    /// Returns the host tool-result contract as JSON text (`{ok, data,
    /// modelData, summary, status, warnings, truncated, limits}`). Call
    /// it from the `registerTool` run function so the durable guard and
    /// authority layers keep judging the call — this host method is for
    /// the JS tool lane, not a bypass around those checks.
    #[napi]
    pub async fn mcp_call_tool(
        &self,
        alias: String,
        tool_name: String,
        args_json: String,
    ) -> Result<String> {
        let arguments: Value = match args_json.trim() {
            "" | "null" => json!({}),
            text => serde_json::from_str(text)
                .map_err(|_| errors::invalid("argsJson must be a JSON object (or empty)"))?,
        };
        let Some(arguments) = arguments.as_object().cloned() else {
            return Err(errors::invalid("argsJson must be a JSON object"));
        };
        // Snapshot under the lock, dispatch WITHOUT it: a slow remote
        // must never serialize other calls or block disconnects behind
        // this binding's own mutex.
        let (manager, canonical, original, valid) = {
            let state = self.mcp.lock().await;
            let registry = state
                .as_ref()
                .ok_or_else(|| errors::stable("not_found", "no MCP server is connected", None))?;
            let entries = registry
                .tools
                .get(&alias)
                .ok_or_else(|| errors::stable("not_found", "unknown MCP server alias", None))?;
            let entry = entries
                .iter()
                .find(|entry| {
                    entry.original == tool_name
                        || entry.effective == tool_name
                        || entry.descriptor.get("name").and_then(Value::as_str)
                            == Some(tool_name.as_str())
                })
                .ok_or_else(|| {
                    errors::stable(
                        "not_found",
                        format!("MCP server {alias} exposes no tool {tool_name}"),
                        None,
                    )
                })?;
            // Fail closed exactly like the crate's RemoteTool: no schema
            // proof, no dispatch.
            let valid = entry
                .validator
                .as_ref()
                .is_some_and(|validator| validator.is_valid(&Value::Object(arguments.clone())));
            (
                Arc::clone(&registry.manager),
                entry.canonical.clone(),
                entry.original.clone(),
                valid,
            )
        };
        if !valid {
            return Err(errors::stable(
                "mcp_invalid_arguments",
                format!(
                    "MCP tool {}.{} was refused by local schema validation",
                    alias, original
                ),
                None,
            ));
        }
        let result = manager
            .call_tool(&canonical, arguments, CancellationToken::new())
            .await
            .map_err(map_client)?;
        Ok(outcome(&canonical, &alias, &original, result).to_string())
    }

    /// Disconnect one MCP server: its tool handles are revoked
    /// immediately, its transport closed, and its declaration dropped.
    /// Idempotent — disconnecting an unknown or already-disconnected
    /// alias is a no-op success.
    #[napi]
    pub async fn mcp_disconnect(&self, alias: String) -> Result<()> {
        let mut state = self.mcp.lock().await;
        let Some(registry) = state.as_mut() else {
            return Ok(());
        };
        if registry.configs.remove(&alias).is_none() {
            registry.tools.remove(&alias);
            return Ok(());
        }
        registry.tools.remove(&alias);
        let remaining = registry.configs.values().cloned().collect::<Vec<_>>();
        registry
            .manager
            .replace_configs(remaining)
            .await
            .map_err(map_client)
    }

    /// Every MCP server declared on this store: `{alias, kind, enabled,
    /// readOnly, connected, tools}`. `connected` is the crate manager's
    /// live-session view (a connected server whose transport dropped
    /// reports false and reconnects on the next host call).
    #[napi]
    pub async fn mcp_list_servers(&self) -> Result<String> {
        let state = self.mcp.lock().await;
        let servers = state
            .as_ref()
            .map(|registry| {
                let connected = registry.manager.connected_aliases();
                registry
                    .configs
                    .values()
                    .map(|config| {
                        json!({
                            "alias": config.alias,
                            "kind": match &config.transport {
                                TransportConfig::Stdio { .. } => "stdio",
                                TransportConfig::Http { .. } => "http",
                            },
                            "enabled": config.enabled,
                            "readOnly": config.read_only,
                            "connected": connected.contains(&config.alias),
                            "tools": registry
                                .tools
                                .get(&config.alias)
                                .map_or(0, Vec::len),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(json!({ "servers": servers }).to_string())
    }
}

impl NativeStore {
    /// Best-effort teardown used by `close()`: revoke every handle and
    /// await transport cleanup. The manager's own `Drop` also reaps any
    /// session the await could not finish, so close never leaks a child
    /// process beyond the handle's lifetime — but unlike a bare drop,
    /// this gives the graceful close a chance to complete first.
    pub(crate) async fn close_mcp(&self) {
        let registry = self.mcp.lock().await.take();
        if let Some(registry) = registry {
            let _ = registry.manager.dispose().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(read_only: bool) -> ServerConfig {
        serde_json::from_value(json!({
            "alias": "notes",
            "transport": {"kind": "stdio", "command": "bun", "args": ["fixture.ts"]},
            "readOnly": read_only,
            "toolAliases": {"write-note": "save-note"},
        }))
        .expect("test config parses")
    }

    fn remote(name: &str, schema: &Value, description: Option<&str>) -> RemoteTool {
        let mut tool = RemoteTool::new(
            name.to_owned(),
            description.unwrap_or("d").to_owned(),
            schema.as_object().cloned().unwrap_or_default(),
        );
        tool.description = description.map(str::to_owned).map(Into::into);
        tool
    }

    #[test]
    fn config_parses_both_transports_and_host_effect_authority() {
        let write = config(false);
        let read = config(true);
        assert!(!write.read_only);
        assert!(read.read_only);
        let http: ServerConfig = serde_json::from_value(json!({
            "alias": "remote",
            "transport": {"kind": "http", "url": "http://127.0.0.1:9/mcp",
                          "headers": {"authorization": "Bearer x"}},
        }))
        .expect("http config parses");
        assert!(matches!(http.transport, TransportConfig::Http { .. }));
        assert!(
            serde_json::from_str::<ServerConfig>("{\"alias\":\"x\"}").is_err(),
            "a config without a transport is not a ServerConfig"
        );
    }

    #[test]
    fn projection_mirrors_the_crate_identity_and_host_effect() {
        let schema =
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
        let write = project_tool(
            "notes",
            &config(false),
            remote("write-note", &schema, Some("Persist a note")),
        )
        .expect("valid tool projects");
        assert_eq!(write.canonical, "mcp.notes.save-note");
        assert_eq!(write.descriptor["name"], "mcp__notes__save-note");
        assert_eq!(write.descriptor["effect"], "write");
        assert_eq!(write.descriptor["capability"], "mcp.notes.save-note");
        assert_eq!(write.descriptor["remoteName"], "write-note");
        assert_eq!(write.descriptor["description"], "Persist a note");
        assert!(write.validator.is_some(), "a sound schema compiles");
        // Host `readOnly:true` is the ONLY way an effect becomes "read".
        let read = project_tool("notes", &config(true), remote("read-note", &schema, None))
            .expect("read-only tool projects");
        assert_eq!(read.descriptor["effect"], "read");
        assert_eq!(
            read.descriptor["description"], "MCP tool read-note from notes",
            "a descriptionless tool keeps the crate's fallback wording"
        );
        // Remote identities outside the crate's grammar are refused.
        assert!(project_tool("notes", &config(false), remote("Bad Name", &schema, None)).is_err());
    }

    #[test]
    fn missing_or_broken_schema_fails_closed() {
        let entry = project_tool(
            "notes",
            &config(false),
            remote("echo", &json!({"type": 42}), None),
        )
        .expect("projection is permissive; validation is what fails closed");
        assert!(entry.validator.is_none());
        let valid = entry
            .validator
            .as_ref()
            .is_some_and(|validator| validator.is_valid(&json!({"text": "x"})));
        assert!(!valid, "no compiled validator means no dispatch, ever");
    }

    #[test]
    fn outcomes_never_launder_a_remote_error() {
        let ok = CallToolResult::success(vec![agentkit_mcp::model::ContentBlock::text("hello")]);
        let value = outcome("mcp.notes.echo", "notes", "echo", ok);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["data"]["text"], json!("hello"));
        assert_eq!(value["data"]["toolName"], json!("echo"));
        assert_eq!(value["status"], json!("ok"));
        let mut failing =
            CallToolResult::success(vec![agentkit_mcp::model::ContentBlock::text("boom")]);
        failing.is_error = Some(true);
        let value = outcome("mcp.notes.echo", "notes", "echo", failing);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["status"], json!(null));
        assert_eq!(value["data"]["isError"], json!(true));
    }
}
