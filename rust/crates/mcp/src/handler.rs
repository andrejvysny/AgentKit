use crate::json_fields::JsonFields;
use crate::{Backend, SessionScope};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use std::{
    borrow::Cow,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

pub(crate) struct ScopeLease {
    pub scope: SessionScope,
    pub backend: Arc<dyn Backend>,
    revoked: AtomicBool,
    pub active: AtomicUsize,
    pub last_used: Mutex<Instant>,
}
impl ScopeLease {
    pub fn new(scope: SessionScope, backend: Arc<dyn Backend>) -> Self {
        Self {
            scope,
            backend,
            revoked: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
        }
    }
    pub fn revoke(&self) {
        if !self.revoked.swap(true, Ordering::SeqCst) {
            self.scope.cancellation.cancel();
            self.backend.revoke_actor(&self.scope.actor_id);
        }
    }
    pub fn begin(self: &Arc<Self>) -> ActiveCall {
        self.active.fetch_add(1, Ordering::SeqCst);
        ActiveCall(self.clone())
    }
}
impl Drop for ScopeLease {
    fn drop(&mut self) {
        self.revoke();
    }
}

pub(crate) struct ActiveCall(Arc<ScopeLease>);
impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        if let Ok(mut instant) = self.0.last_used.lock() {
            *instant = Instant::now();
        }
    }
}

#[derive(Clone)]
pub(crate) struct NativeHandler {
    backend: Arc<dyn Backend>,
    scope: Arc<OnceLock<Arc<ScopeLease>>>,
    writes_enabled: bool,
    deadline: Duration,
    calls: Arc<Semaphore>,
    page_size: usize,
}

impl NativeHandler {
    pub fn new(backend: Arc<dyn Backend>, options: &crate::ServerOptions) -> Self {
        Self {
            backend,
            scope: Arc::default(),
            writes_enabled: options.writes_enabled,
            deadline: options.max_call_duration,
            calls: Arc::new(Semaphore::new(options.max_concurrent_calls)),
            page_size: options.tool_page_size,
        }
    }
    fn scope(&self) -> Result<SessionScope, ErrorData> {
        let mut scope = self
            .scope
            .get()
            .map(|lease| lease.scope.clone())
            .ok_or_else(|| ErrorData::invalid_request("MCP session not initialized", None))?;
        scope.cancellation = scope.cancellation.child_token();
        Ok(scope)
    }
    fn ensure_live(
        &self,
        context: &RequestContext<RoleServer>,
        scope: &SessionScope,
    ) -> Result<(), ErrorData> {
        let lease = self.scope.get().ok_or_else(internal)?;
        if lease.revoked.load(Ordering::SeqCst) || scope.cancellation.is_cancelled() {
            return Err(ErrorData::invalid_request("MCP session closed", None));
        }
        if context.ct.is_cancelled() {
            return Err(ErrorData::invalid_request("MCP request cancelled", None));
        }
        Ok(())
    }
    async fn bounded<T>(
        &self,
        context: &RequestContext<RoleServer>,
        scope: &SessionScope,
        operation: impl Future<Output = Result<T, ErrorData>>,
    ) -> Result<T, ErrorData>
    where
        T: JsonFields,
    {
        let _cancel_on_return = scope.cancellation.clone().drop_guard();
        let lease = self.scope.get().ok_or_else(internal)?;
        let _active = lease.begin();
        let work = async {
            let _permit = self.calls.acquire().await.map_err(|_| internal())?;
            self.ensure_live(context, scope)?;
            operation.await
        };
        let result = tokio::select! {
            biased;
            _ = scope.cancellation.cancelled() => Err(ErrorData::invalid_request("MCP session closed", None)),
            _ = context.ct.cancelled() => Err(ErrorData::invalid_request("MCP request cancelled", None)),
            result = tokio::time::timeout(self.deadline, work) => result.unwrap_or_else(|_| Err(internal())),
        };
        match result {
            Ok(value) => Ok(wire_value(value)),
            Err(error) => Err(wire_value(error)),
        }
    }
}

impl ServerHandler for NativeHandler {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(crate::protocol_versions())
    }
    fn get_info(&self) -> ServerConfig {
        let features = self.backend.features();
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        if features.resources {
            capabilities.resources = Some(Default::default());
        }
        if features.prompts {
            capabilities.prompts = Some(Default::default());
        }
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("agentkit-mcp-server", "0.1.0"))
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        if !crate::TS_PROTOCOL_VERSIONS.contains(&request.protocol_version.as_str()) {
            return Err(ErrorData::invalid_request(
                "Unsupported MCP protocol version",
                None,
            ));
        }
        let lease = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Arc<ScopeLease>>())
            .cloned()
            .ok_or_else(|| ErrorData::invalid_request("Authenticated scope required", None))?;
        self.scope
            .set(lease)
            .map_err(|_| ErrorData::invalid_request("Already initialized", None))?;
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }
    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "Method not found",
            None,
        ))
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let scope = self.scope()?;
        self.bounded(&context, &scope, async {
            let entries = self.backend.list_tools(&scope).await?;
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|entry| self.writes_enabled || !entry.writes)
                .collect();
            let start = page_start(
                request
                    .as_ref()
                    .and_then(|request| request.cursor.as_deref()),
                entries.len(),
            )?;
            let mut result = ListToolsResult::with_all_items(
                entries
                    .iter()
                    .skip(start)
                    .take(self.page_size)
                    .map(|entry| {
                        let mut tool = entry.definition.clone();
                        tool.output_schema = None;
                        tool
                    })
                    .collect(),
            );
            if start + result.tools.len() < entries.len() {
                result.next_cursor = Some((start + result.tools.len()).to_string());
            }
            Ok(result)
        })
        .await
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let scope = self.scope()?;
        self.bounded(&context, &scope, async {
            let entries = self.backend.list_tools(&scope).await?;
            let tool = entries
                .into_iter()
                .find(|entry| {
                    entry.definition.name == request.name && (self.writes_enabled || !entry.writes)
                })
                .ok_or_else(|| ErrorData::invalid_params("Unknown tool", None))?;
            let mut arguments = request.arguments.unwrap_or_default();
            crate::json_boundary::map_object(&mut arguments, crate::json_boundary::restore);
            let schema = serde_json::Value::Object((*tool.definition.input_schema).clone());
            let validator =
                agentkit_contracts::compile_tool_schema(&schema).map_err(|_| internal())?;
            if !validator.is_valid(&serde_json::Value::Object(arguments.clone())) {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "Invalid tool arguments",
                )]));
            }
            // A catalogue can cancel and return Ready in the same select poll.
            self.ensure_live(&context, &scope)?;
            self.backend
                .call_tool(&request.name, arguments, &scope)
                .await
        })
        .await
        .map(Into::into)
    }
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let scope = self.scope()?;
        self.bounded(&context, &scope, async {
            if !self.backend.features().resources {
                return Err(unavailable());
            }
            let mut result = ListResourcesResult::default();
            result.resources = self.backend.list_resources(&scope).await?;
            Ok(result)
        })
        .await
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let scope = self.scope()?;
        self.bounded(&context, &scope, async {
            if !self.backend.features().resources {
                return Err(unavailable());
            }
            if !self
                .backend
                .list_resources(&scope)
                .await?
                .iter()
                .any(|resource| resource.uri == request.uri)
            {
                return Err(ErrorData::invalid_params("Unknown resource", None));
            }
            self.ensure_live(&context, &scope)?;
            self.backend.read_resource(&request.uri, &scope).await
        })
        .await
        .map(Into::into)
    }
    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let scope = self.scope()?;
        self.bounded(&context, &scope, async {
            if !self.backend.features().prompts {
                return Err(unavailable());
            }
            let mut result = ListPromptsResult::default();
            result.prompts = self.backend.list_prompts(&scope).await?;
            Ok(result)
        })
        .await
    }
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let scope = self.scope()?;
        self.bounded(&context, &scope, async {
            if !self.backend.features().prompts {
                return Err(unavailable());
            }
            let prompt = self
                .backend
                .list_prompts(&scope)
                .await?
                .into_iter()
                .find(|prompt| prompt.name == request.name)
                .ok_or_else(|| ErrorData::invalid_params("Unknown prompt", None))?;
            let mut arguments = request.arguments.unwrap_or_default();
            crate::json_boundary::map_object(&mut arguments, crate::json_boundary::restore);
            let allowed = prompt.arguments.unwrap_or_default();
            if arguments.iter().any(|(name, value)| {
                !value.is_string() || !allowed.iter().any(|item| item.name == *name)
            }) || allowed
                .iter()
                .any(|item| item.required == Some(true) && !arguments.contains_key(&item.name))
            {
                return Err(ErrorData::invalid_params("Invalid prompt arguments", None));
            }
            self.ensure_live(&context, &scope)?;
            self.backend
                .get_prompt(&request.name, arguments, &scope)
                .await
        })
        .await
        .map(Into::into)
    }
}

fn page_start(cursor: Option<&str>, length: usize) -> Result<usize, ErrorData> {
    let start = match cursor {
        None => 0,
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| ErrorData::invalid_params("Invalid cursor", None))?,
    };
    if start > length {
        return Err(ErrorData::invalid_params("Invalid cursor", None));
    }
    Ok(start)
}
fn wire_value<T: JsonFields>(mut value: T) -> T {
    value.map_json(agentkit_contracts::javascript_json_value);
    value
}
fn internal() -> ErrorData {
    ErrorData::internal_error("Host MCP operation failed", None)
}
fn unavailable() -> ErrorData {
    ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", None)
}
