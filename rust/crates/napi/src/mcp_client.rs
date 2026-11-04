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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use agentkit_core::{ToolLimits, ToolResult};
use agentkit_mcp::model::{CallToolResult, Tool as RemoteTool};
use agentkit_mcp::{
    McpClientError, McpClientManager, McpSecretResolver, ServerConfig, ToolBinding, TransportConfig,
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
    /// How many times each alias has been (re)registered on this store. It
    /// never goes down — not even on `mcpDisconnect` — so a descriptor from a
    /// superseded registration can never be passed off as the current one.
    registrations: HashMap<String, u64>,
    /// In-flight `mcpCallTool` invocations by the cancellation key the JS
    /// lane minted. `mcpCancelCall` tears one down (abort of the originating
    /// invocation); `mcpCallTool` registers the token before any await that
    /// can dispatch and removes it when the call settles.
    pending: HashMap<String, CancellationToken>,
    /// Cancellation keys whose cancel arrived BEFORE the call registered its
    /// token (the napi future had not been polled yet). Registration consumes
    /// the tombstone into an already-cancelled token, so an abort that wins
    /// the race still denies the dispatch — the key never dispatches.
    cancelled: HashSet<String>,
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
    /// The registration that minted this descriptor, in the form handed to JS
    /// as `descriptor.handle` and required back by `mcpCallTool`.
    handle: String,
    /// The exact transport this descriptor was LISTED from. Dispatch is
    /// pinned to it (crate `ToolBinding`): a descriptor from generation N
    /// never executes a reconnect's generation N+1 handler — that fails
    /// `mcp_authority_revoked` at the session pin and again at the
    /// wire-last bound guard. `None` only for a server that had no live
    /// transport at projection time, which `mcpConnect` refuses outright.
    binding: Option<ToolBinding>,
}

/// Resolve the addressable name a caller used for one alias's cached tools.
/// Precedence is explicit — the remote's ORIGINAL name first, then the full
/// descriptor name, then a configured `toolAliases` target — so resolution
/// can never depend on the order the listing happened to arrive in. The
/// connect-time gates (`project_tool`, mirroring `bridge::resolve_original`
/// and `bridge::aliases_are_one_step`) make those three domains disjoint,
/// which is what makes one entry per name provable; the ordering here is
/// defense in depth for a cache built by any other path.
fn resolve_entry<'a>(entries: &'a [CachedTool], tool_name: &str) -> Option<&'a CachedTool> {
    entries
        .iter()
        .find(|entry| entry.original == tool_name)
        .or_else(|| {
            entries.iter().find(|entry| {
                entry.descriptor.get("name").and_then(Value::as_str) == Some(tool_name)
            })
        })
        .or_else(|| entries.iter().find(|entry| entry.effective == tool_name))
}

/// The opaque handle one registration issues. Opaque to JS, but it names the
/// (alias, registration) pair, so a stale descriptor cannot be repaired by
/// editing it: only the value `mcpConnect` returned for the CURRENT
/// registration of that alias is admitted.
fn handle_for(alias: &str, registration: u64) -> String {
    format!("{alias}#{registration}")
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
    registration: u64,
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
    // Same one-step gate the crate applies when it constructs a tool set: a
    // chain (`echo -> shadow` alongside `write_note -> echo`) makes one name
    // denote two tools, so the descriptor cache — the catalogue the host and
    // every JS caller address — could advertise what its resolver reaches
    // elsewhere. Refused at connect, before any descriptor is handed out.
    if !agentkit_mcp::aliases_are_one_step(&config.tool_aliases) {
        return Err(McpClientError::Configuration);
    }
    // The crate's own inverse (`bridge::resolve_original`): a name this config
    // has renamed away is not this server's address, so a descriptor must never
    // advertise a canonical id the crate would route elsewhere.
    if agentkit_mcp::resolve_original(&config.tool_aliases, &effective)
        .is_none_or(|source| source != original)
    {
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
        "handle": handle_for(alias, registration),
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
        handle: handle_for(alias, registration),
        // Stamped by `mcpConnect` from the session's live transport right
        // after the listing that produced this entry.
        binding: None,
    })
}

/// The cross-namespace addressability gate (external review section-3 row):
/// `mcpCallTool` resolves a name per alias as ORIGINAL → descriptor name →
/// EFFECTIVE. If one query string is claimed by TWO entries of the same
/// alias — e.g. a remote tool literally named `mcp__<alias>__echo` beside a
/// tool `echo` whose advertised descriptor name is exactly that — the
/// precedence order would advertise one handler and dispatch another. Such
/// a catalogue is refused at connect, before any descriptor is handed out,
/// so no advertised name can ever reach a foreign handler.
fn catalogue_is_addressable(entries: &[CachedTool]) -> bool {
    let mut claimed: HashMap<&str, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let descriptor = entry
            .descriptor
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for query in [
            entry.original.as_str(),
            descriptor,
            entry.effective.as_str(),
        ] {
            match claimed.get(query) {
                Some(seen) if *seen != index => return false,
                Some(_) => {}
                None => {
                    claimed.insert(query, index);
                }
            }
        }
    }
    true
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

        model_content: None,
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
        // Connects are serialized by their own gate so the registry mutex is
        // never held across spawn / handshake / `tools/list` (which can take
        // as long as a server cares to stall): calls, cancels, and
        // disconnects on OTHER aliases keep flowing meanwhile.
        let _connect_gate = self.mcp_connect_gate.lock().await;
        let alias = config.alias.clone();
        // Phase 1 (short lock): validate, snapshot, mint the registration.
        let (manager, registration, known_aliases, next_configs, mut names, mut canonicals) = {
            let mut state = self.mcp.lock().await;
            if state.is_none() {
                let manager = McpClientManager::new(Vec::new(), Arc::new(UnavailableSecrets))
                    .map_err(map_client)?;
                *state = Some(Registry {
                    manager: Arc::new(manager),
                    configs: BTreeMap::new(),
                    tools: HashMap::new(),
                    registrations: HashMap::new(),
                    pending: HashMap::new(),
                    cancelled: HashSet::new(),
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
            // Build the NEXT state completely before publishing any of it.
            // The mirrors (`configs`, `tools`, `registrations`) are written
            // only once the new server has connected AND projected, so every
            // failure path leaves the servers that were already connected
            // exactly as they were. A reconnect under a known alias is a NEW
            // registration: it mints new handles, and the previous
            // registration's handles stop being admitted.
            let registration = registry
                .registrations
                .get(&alias)
                .copied()
                .unwrap_or(0)
                .wrapping_add(1);
            let mut next_configs = registry.configs.clone();
            next_configs.insert(alias.clone(), config.clone());
            // Cross-server descriptor uniqueness is checked against the
            // CURRENT cache plus the candidate set, before publishing.
            let names: Vec<String> = registry
                .tools
                .values()
                .flatten()
                .filter_map(|entry| entry.descriptor.get("name")?.as_str().map(str::to_owned))
                .collect();
            let canonicals: Vec<String> = registry
                .tools
                .values()
                .flatten()
                .map(|entry| entry.canonical.clone())
                .collect();
            let known_aliases: Vec<String> = registry.configs.keys().cloned().collect();
            (
                Arc::clone(&registry.manager),
                registration,
                known_aliases,
                next_configs,
                names,
                canonicals,
            )
        };
        // Phase 2 (no registry lock): the network work.
        let cancellation = CancellationToken::new();
        let projected = async {
            manager
                .replace_configs(next_configs.values().cloned().collect())
                .await?;
            manager.connect(&alias, cancellation.clone()).await?;
            let listed = manager.list_tools(&alias, cancellation).await?;
            // Capture the transport the listing just answered on: every
            // descriptor minted here is pinned to it (W1d - bound dispatch
            // across automatic reconnects). A session that lost its
            // transport between the listing and this read is refused here;
            // an unbound descriptor would silently dispatch on whichever
            // transport the session happens to hold at call time.
            let binding = manager.binding(&alias).ok_or(McpClientError::Closed)?;
            let mut projected = Vec::new();
            for tool in listed {
                let mut entry = project_tool(&alias, &config, tool, registration)?;
                entry.binding = Some(binding);
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
            // Cross-namespace ambiguity (an advertised name that is another
            // tool's ORIGINAL) is refused here, before a single descriptor of
            // this registration is published.
            if !catalogue_is_addressable(&projected) {
                return Err(McpClientError::Configuration);
            }
            Ok::<_, McpClientError>(projected)
        }
        .await;
        // Phase 3 (short lock): publish, or roll the manager back to the
        // declaration set the registry actually holds.
        let mut state = self.mcp.lock().await;
        let projected = match projected {
            Ok(projected) => projected,
            Err(error) => {
                if let Some(registry) = state.as_ref() {
                    let rollback = registry.configs.values().cloned().collect::<Vec<_>>();
                    let _ = registry.manager.replace_configs(rollback).await;
                }
                return Err(map_client(error));
            }
        };
        let Some(registry) = state.as_mut() else {
            // The store closed while the connect was in flight; its teardown
            // already closed the manager, so there is nothing to publish.
            return Err(errors::closed("the store is closed"));
        };
        if !registry.configs.keys().eq(known_aliases.iter()) {
            // A disconnect landed during the handshake: the manager was
            // configured from a stale declaration set. Restore it and make
            // the caller retry against the registry as it now stands.
            let rollback = registry.configs.values().cloned().collect::<Vec<_>>();
            let _ = registry.manager.replace_configs(rollback).await;
            return Err(errors::stable(
                "invalid_state",
                "the connected MCP server set changed while connecting; retry mcpConnect()",
                None,
            ));
        }
        registry.configs = next_configs;
        registry.tools.insert(alias.clone(), projected);
        registry.registrations.insert(alias.clone(), registration);
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
    /// descriptor name. `handle` is the descriptor's `handle` field, i.e. the
    /// registration that minted it: it is REQUIRED once an alias has been
    /// re-registered (disconnect + reconnect under the same alias mints a new
    /// registration), and a call whose handle names a superseded registration
    /// is refused with `mcp_authority_revoked` — a descriptor from generation
    /// N never authorizes generation N+1's callback. Dispatch is additionally
    /// PINNED to the transport the descriptor was listed from (crate
    /// `ToolBinding`): after an automatic reconnect the stale descriptor is
    /// refused with `mcp_authority_revoked` instead of executing the new
    /// generation's handler, and the pin is re-checked wire-last, after any
    /// reconnect, secret resolution, or backoff, immediately before the
    /// request is written. The arguments are
    /// validated LOCALLY against the descriptor's recorded input schema before
    /// dispatch (invalid arguments never reach the server,
    /// `mcp_invalid_arguments`; a schema that could not be compiled fails
    /// closed the same way).
    /// `cancellationKey` is the abort handle the JS tool lane mints for one
    /// originating invocation: while the key is registered, `mcpCancelCall`
    /// tears the in-flight call down (pre-dispatch it is never written to the
    /// wire — connect, reconnect, backoff and the wire-last guard all observe
    /// the token — and mid-flight it aborts like any crate cancellation,
    /// which keeps its ambiguous-delivery semantics).
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
        handle: Option<String>,
        cancellation_key: Option<String>,
    ) -> Result<String> {
        // Register the abort handle FIRST — before any fallible parse — so
        // an `mcpCancelCall` that wins the race can only land as a tombstone
        // this registration consumes, never as a cancel that misses the call.
        let token = {
            let mut state = self.mcp.lock().await;
            match (state.as_mut(), cancellation_key.as_deref()) {
                (Some(registry), Some(key)) => {
                    let token = CancellationToken::new();
                    if registry.cancelled.remove(key) {
                        token.cancel();
                    }
                    registry.pending.insert(key.to_owned(), token.clone());
                    token
                }
                _ => CancellationToken::new(),
            }
        };
        let dispatched = Self::dispatch_mcp_call(
            self,
            &alias,
            &tool_name,
            &args_json,
            handle.as_deref(),
            token,
        )
        .await;
        if let Some(key) = cancellation_key.as_deref() {
            let mut state = self.mcp.lock().await;
            if let Some(registry) = state.as_mut() {
                registry.pending.remove(key);
            }
        }
        dispatched
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch_mcp_call(
        &self,
        alias: &str,
        tool_name: &str,
        args_json: &str,
        handle: Option<&str>,
        token: CancellationToken,
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
        let (manager, canonical, original, valid, admitted, binding) = {
            let state = self.mcp.lock().await;
            let registry = state
                .as_ref()
                .ok_or_else(|| errors::stable("not_found", "no MCP server is connected", None))?;
            let entries = registry
                .tools
                .get(alias)
                .ok_or_else(|| errors::stable("not_found", "unknown MCP server alias", None))?;
            let entry = resolve_entry(entries, tool_name).ok_or_else(|| {
                errors::stable(
                    "not_found",
                    format!("MCP server {alias} exposes no tool {tool_name}"),
                    None,
                )
            })?;
            // Registration admission, checked against the very entry this call
            // is about to execute: the handle the caller presents must be the
            // one that minted it. Names alone cannot settle that — after a
            // reconnect under the same alias, the superseded descriptor and the
            // current one share name, capability and remoteName.
            let registration = registry.registrations.get(alias).copied().unwrap_or(0);
            let admitted = match handle {
                Some(presented) => presented == entry.handle,
                // An alias registered exactly once has no earlier generation to
                // be confused with, so the legacy three-argument call is exact.
                None => registration <= 1,
            };
            // Fail closed exactly like the crate's RemoteTool: no schema
            // proof, no dispatch.
            let valid = entry
                .validator
                .as_ref()
                .is_some_and(|validator| validator.is_valid(&Value::Object(arguments.clone())));
            // Every descriptor mints with its listing's transport; an entry
            // without one could only come from a projection that never
            // published. Fail closed rather than dispatch unbound.
            let binding = entry
                .binding
                .ok_or_else(|| errors::internal("MCP descriptor carries no transport binding"))?;
            (
                Arc::clone(&registry.manager),
                entry.canonical.clone(),
                entry.original.clone(),
                valid,
                admitted,
                binding,
            )
        };
        if !admitted {
            return Err(errors::stable(
                "mcp_authority_revoked",
                format!(
                    "MCP tool {alias}.{tool_name} was addressed with a handle from a superseded \
                     registration of server {alias}; use the descriptor handle mcpConnect() \
                     returned for the current registration"
                ),
                None,
            ));
        }
        if !valid {
            return Err(errors::stable(
                "mcp_invalid_arguments",
                format!("MCP tool {alias}.{original} was refused by local schema validation",),
                None,
            ));
        }
        let result = manager
            .call_tool_bound(&canonical, binding, arguments, token)
            .await
            .map_err(map_client)?;
        Ok(outcome(&canonical, alias, &original, result).to_string())
    }

    /// Tear down one in-flight `mcpCallTool` by the cancellation key the JS
    /// lane minted: this is the binding half of the originating invocation's
    /// AbortSignal. Pre-dispatch the request is never written (connect,
    /// reconnect, backoff and the wire-last guard all observe the token);
    /// after the request left, the call aborts like any crate cancellation.
    /// Cancelling an unknown or already-settled key is a no-op success.
    #[napi]
    pub async fn mcp_cancel_call(&self, cancellation_key: String) -> Result<()> {
        let mut state = self.mcp.lock().await;
        if let Some(registry) = state.as_mut() {
            match registry.pending.remove(&cancellation_key) {
                Some(token) => token.cancel(),
                // The call's future has not registered yet (or already
                // settled): remember the abort so registration — if it is
                // still coming — starts out cancelled.
                None => {
                    registry.cancelled.insert(cancellation_key);
                }
            }
        }
        Ok(())
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
        if !registry.configs.contains_key(&alias) {
            registry.tools.remove(&alias);
            return Ok(());
        }
        // Compute the next set first; the mirrors are only edited once the
        // manager has accepted it. A failure here means the manager could not
        // CLOSE the dropped transport — the session was already revoked and
        // removed either way, so the mirrors must match the manager, not the
        // declaration the call started from.
        let remaining = registry
            .configs
            .values()
            .filter(|config| config.alias != alias)
            .cloned()
            .collect::<Vec<_>>();
        let failure = registry
            .manager
            .replace_configs(remaining)
            .await
            .err()
            .map(map_client);
        registry.configs.remove(&alias);
        registry.tools.remove(&alias);
        failure.map_or(Ok(()), Err)
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
                            "registration": registry
                                .registrations
                                .get(&config.alias)
                                .copied()
                                .unwrap_or(0),
                            "handle": registry
                                .registrations
                                .get(&config.alias)
                                .copied()
                                .map(|registration| handle_for(&config.alias, registration)),
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

    fn alias_config(aliases: Value) -> ServerConfig {
        serde_json::from_value(json!({
            "alias": "dual",
            "transport": {"kind": "stdio", "command": "bun", "args": ["fixture.ts"]},
            "toolAliases": aliases,
        }))
        .expect("test config parses")
    }

    /// A replacement that is itself a renamed-away original (`echo -> shadow`
    /// alongside `write_note -> echo`) makes ONE name denote two tools: the
    /// forward map reaches `echo`, an inverse lookup reaches `write_note`.
    /// The crate refuses that in `McpToolSet::new`; this mirror must refuse it
    /// too, or the cache the host addresses could advertise a callback its
    /// resolver reaches elsewhere.
    #[test]
    fn an_alias_chain_is_refused_before_any_descriptor_is_minted() {
        let schema =
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
        let chained = alias_config(json!({"echo": "shadow", "write_note": "echo"}));
        assert!(
            matches!(
                project_tool("dual", &chained, remote("echo", &schema, None), 1),
                Err(McpClientError::Configuration)
            ),
            "the renamed-away tool must not be projected"
        );
        assert!(
            matches!(
                project_tool("dual", &chained, remote("write_note", &schema, None), 1),
                Err(McpClientError::Configuration)
            ),
            "the chain is a property of the map, so no projection of it survives"
        );
        // A legal one-step rename still works, and renames ONLY the advertised
        // identity — the route back stays the remote's own name.
        let legal = alias_config(json!({"echo": "shadow", "write_note": "note"}));
        let entry = project_tool("dual", &legal, remote("echo", &schema, None), 3)
            .expect("a one-step rename projects");
        assert_eq!(entry.effective, "shadow");
        assert_eq!(entry.original, "echo");
        assert_eq!(entry.canonical, "mcp.dual.shadow");
        assert_eq!(entry.descriptor["remoteName"], "echo");
        assert_eq!(entry.handle, "dual#3");
    }

    fn cached(original: &str, effective: &str, handle: &str) -> CachedTool {
        let canonical = format!("mcp.s.{effective}");
        CachedTool {
            original: original.into(),
            effective: effective.into(),
            descriptor: json!({
                "name": canonical.replace('.', "__"),
                "remoteName": original,
                "handle": handle,
            }),
            canonical,
            validator: None,
            handle: handle.into(),
            binding: None,
        }
    }

    /// Resolution is a function of the name, never of the order the listing
    /// arrived in: the remote's ORIGINAL name wins over any alias target that
    /// happens to equal it, descriptor names resolve to their own entry, and
    /// an alias target reaches the entry that advertised it.
    #[test]
    fn resolution_never_routes_by_the_order_the_listing_arrived_in() {
        // A cache in which one tool's alias TARGET equals another tool's
        // ORIGINAL name — the ordering hazard the connect gates exclude, so
        // the resolver must still be order-independent. `note` is renamed to
        // "echo" and is listed BEFORE the genuine `echo`.
        let entries = vec![cached("note", "echo", "s#1"), cached("echo", "echo", "s#1")];
        assert_eq!(resolve_entry(&entries, "echo").unwrap().original, "echo");
        assert_eq!(resolve_entry(&entries, "note").unwrap().original, "note");
        // A legal cache: every addressable name resolves to exactly one entry.
        let entries = vec![cached("note", "memo", "s#2"), cached("echo", "echo", "s#2")];
        assert_eq!(resolve_entry(&entries, "note").unwrap().effective, "memo");
        assert_eq!(resolve_entry(&entries, "memo").unwrap().original, "note");
        assert_eq!(
            resolve_entry(&entries, "mcp__s__memo").unwrap().original,
            "note",
            "the full descriptor name addresses its own entry"
        );
        assert!(resolve_entry(&entries, "nope").is_none());
    }

    #[test]
    fn projection_mirrors_the_crate_identity_and_host_effect() {
        let schema =
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
        let write = project_tool(
            "notes",
            &config(false),
            remote("write-note", &schema, Some("Persist a note")),
            1,
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
        let read = project_tool(
            "notes",
            &config(true),
            remote("read-note", &schema, None),
            1,
        )
        .expect("read-only tool projects");
        assert_eq!(read.descriptor["effect"], "read");
        assert_eq!(
            read.descriptor["description"], "MCP tool read-note from notes",
            "a descriptionless tool keeps the crate's fallback wording"
        );
        // Remote identities outside the crate's grammar are refused.
        assert!(
            project_tool(
                "notes",
                &config(false),
                remote("Bad Name", &schema, None),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn missing_or_broken_schema_fails_closed() {
        let entry = project_tool(
            "notes",
            &config(false),
            remote("echo", &json!({"type": 42}), None),
            1,
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

    /// The W1e gate: a remote name that is another tool's advertised
    /// descriptor name makes one query string denote two handlers, and the
    /// ORIGINAL-first resolver would advertise one and run the other. The
    /// catalogue is refused while it is still unpublished.
    #[test]
    fn cross_namespace_ambiguity_is_refused_before_the_catalogue_publishes() {
        let schema =
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
        let names = alias_config(json!({}));
        let echo =
            project_tool("names", &names, remote("echo", &schema, None), 1).expect("echo projects");
        let note = project_tool("names", &names, remote("write_note", &schema, None), 1)
            .expect("write_note projects");
        assert!(
            catalogue_is_addressable(&[echo, note]),
            "a healthy catalogue addresses every query to exactly one entry"
        );

        let echo = project_tool("names", &names, remote("echo", &schema, None), 1).unwrap();
        let shadow = project_tool(
            "names",
            &names,
            remote("mcp__names__echo", &schema, None),
            1,
        )
        .expect("the foreign name is itself a legal identity");
        assert_eq!(
            echo.descriptor["name"],
            shadow.original.as_str(),
            "the repro is real: echo's advertised name IS the shadow's remote name"
        );
        assert!(
            !catalogue_is_addressable(&[echo, shadow]),
            "one query string denoting two handlers must refuse the whole catalogue"
        );
    }
}
