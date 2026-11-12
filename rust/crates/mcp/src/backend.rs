use async_trait::async_trait;
use rmcp::{ErrorData, model::*};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A type-preserving JSON-RPC 2.0 request id (review R7, AGENTKIT-115
/// lane).
///
/// JSON-RPC 2.0 ids are `string | number | null`. Stringifying an id
/// before using it as an identity component collapses numeric `1` and
/// string `"1"` into one key, so two provably DIFFERENT calls would share
/// one memoized tool result (review R7, reproduced). The JSON type lives
/// in the variant, so ids of different kinds never compare equal and
/// never collide in a memo map: this type is `Hash + Eq` and must be
/// stored typed — never re-stringified — wherever an id proves a
/// redelivery.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RpcId {
    /// Numeric id in the i64 range — the transport's own representation
    /// (`rmcp::model::NumberOrString::Number`).
    Number(i64),
    /// String id, preserved verbatim: no normalization, ever.
    String(String),
    /// Any other well-formed JSON id value (`null`, non-integral numbers,
    /// composites). Carries canonical JSON text, so it can never collide
    /// with a [`RpcId::Number`] or [`RpcId::String`] id. The rmcp
    /// transport rejects such ids on the wire, so this variant exists for
    /// consumers that hold the raw JSON form and must keep the same typed
    /// distinction instead of stringifying.
    Other(String),
}

impl RpcId {
    /// Lossless extraction of a transport request id (review R7): matches
    /// the typed wire variant directly — never `serde_json::to_value` +
    /// `to_string`, which is exactly the collapse this type forbids.
    #[must_use]
    pub fn from_request_id(id: &NumberOrString) -> Self {
        match id {
            NumberOrString::Number(number) => RpcId::Number(*number),
            NumberOrString::String(text) => RpcId::String(text.to_string()),
        }
    }

    /// Typed id from a raw JSON value, for consumers and tests that hold
    /// the wire form directly.
    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::String(text) => RpcId::String(text.clone()),
            Value::Number(number) => number
                .as_i64()
                .map_or_else(|| RpcId::Other(number.to_string()), RpcId::Number),
            other => RpcId::Other(other.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionScope {
    pub principal: Value,
    pub chat_id: Option<String>,
    pub actor_id: String,
    /// JSON-RPC id of the EXACT request that produced this call, when the
    /// transport had one, kept TYPED (review R7): numeric `1` and string
    /// `"1"` are different ids. Backends use it to distinguish a proven
    /// client redelivery (same id) from an independent new call; identity
    /// of a call itself stays a fresh per-dispatch value (AGENTKIT-115).
    /// It is never sole proof of identity on its own: a memo keyed on it
    /// must also carry the session identity (`actor_id`).
    pub request_id: Option<RpcId>,
    pub cancellation: CancellationToken,
}

#[derive(Clone)]
pub struct CatalogTool {
    pub definition: Tool,
    /// Host-owned effect, never inferred from untrusted MCP annotations.
    pub writes: bool,
}

#[derive(Clone, Copy, Default)]
pub struct Features {
    pub resources: bool,
    pub prompts: bool,
}

/// Implementations must stage the same catalogue/guards used by the local host.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn features(&self) -> Features {
        Features::default()
    }
    async fn list_tools(&self, scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData>;
    async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData>;
    /// Must revoke immediately; close paths invoke this before reporting success.
    fn revoke_actor(&self, actor_id: &str);
    async fn list_resources(&self, _scope: &SessionScope) -> Result<Vec<Resource>, ErrorData> {
        Ok(vec![])
    }
    async fn read_resource(
        &self,
        _uri: &str,
        _scope: &SessionScope,
    ) -> Result<ReadResourceResult, ErrorData> {
        Err(ErrorData::invalid_params("Unknown resource", None))
    }
    async fn list_prompts(&self, _scope: &SessionScope) -> Result<Vec<Prompt>, ErrorData> {
        Ok(vec![])
    }
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: JsonObject,
        _scope: &SessionScope,
    ) -> Result<GetPromptResult, ErrorData> {
        Err(ErrorData::invalid_params("Unknown prompt", None))
    }
}
