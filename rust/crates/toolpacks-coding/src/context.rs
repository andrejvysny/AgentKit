//! Per-pack environment and the tool wrapper. Mirrors
//! `packages/toolpacks-coding/src/context.ts`.

use std::collections::HashMap;
use std::sync::Arc;

use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::coordination::WorkspaceCoordinator;
use crate::errors::CodingError;
use crate::limits::CodingLimits;
use crate::paths::canonical_root;
use crate::types::{Clock, SandboxMode};

use agentkit_sandbox_contracts::grant::WriterLeaseToken;
use agentkit_sandbox_contracts::session::VmSandboxSession;

/// What every tool body needs; built once per pack.
pub struct PackEnv {
    pub workspace_root: String,
    pub limits: CodingLimits,
    pub allow_sensitive: bool,
    pub clock: Clock,
    pub sandbox: SandboxMode,
    pub network: bool,
    pub allow_git_writes: bool,
    pub test_commands: Vec<String>,
    pub platform: String,
    pub sandbox_exec_path: String,
    pub coordinator: Arc<dyn WorkspaceCoordinator>,
    /// Longest a mutation waits for a lease before `workspace_busy`.
    pub coordination_wait_ms: u64,
    /// Base environment for spawned commands; `None` = host process env.
    pub environment: Option<Arc<HashMap<String, String>>>,
    /// The VM sandbox session for this pack. `Some` routes every
    /// sandbox-bound tool call (fs, search, git inspect, exec) through
    /// `session.io` and NEVER touches host I/O (S06 — no host fallback,
    /// including after a guest error or timeout). `None` keeps today's
    /// host behaviour bit-identical.
    pub vm: Option<Arc<VmSandboxSession>>,
    /// Writer-lease injection point for the VM session (S09). Mutating
    /// routed calls (`io.write`, `io.exec`) pass this token as the grant's
    /// `writer_lease`; `None` (the default) is what v1 sends — the host
    /// bridge mints the admitted writer token after durable writer
    /// admission and sets this field on the `PackEnv` before sharing the
    /// pack's `Arc`. The pack never mints, widens or fabricates a lease
    /// (S08), and never edits the contracts to obtain one.
    pub vm_writer_lease: Option<WriterLeaseToken>,
    root: Mutex<Option<Result<String, CodingError>>>,
    scratch: Mutex<Option<Result<String, CodingError>>>,
    closed: agentkit_core::CancellationToken,
}

#[derive(Clone)]
pub struct EnvInit {
    pub workspace_root: String,
    pub limits: CodingLimits,
    pub allow_sensitive: bool,
    pub clock: Clock,
    pub sandbox: SandboxMode,
    pub network: bool,
    pub allow_git_writes: bool,
    pub test_commands: Vec<String>,
    pub platform: String,
    pub sandbox_exec_path: String,
    pub coordinator: Arc<dyn WorkspaceCoordinator>,
    pub coordination_wait_ms: u64,
    pub environment: Option<Arc<HashMap<String, String>>>,
    /// See [`PackEnv::vm`]; default `None` = host behaviour.
    pub vm: Option<Arc<VmSandboxSession>>,
}

impl PackEnv {
    pub fn new(init: EnvInit) -> Self {
        Self {
            workspace_root: init.workspace_root,
            limits: init.limits,
            allow_sensitive: init.allow_sensitive,
            clock: init.clock,
            sandbox: init.sandbox,
            network: init.network,
            allow_git_writes: init.allow_git_writes,
            test_commands: init.test_commands,
            platform: init.platform,
            sandbox_exec_path: init.sandbox_exec_path,
            coordinator: init.coordinator,
            coordination_wait_ms: init.coordination_wait_ms,
            environment: init.environment,
            vm: init.vm,
            vm_writer_lease: None,
            root: Mutex::new(None),
            scratch: Mutex::new(None),
            closed: agentkit_core::CancellationToken::new(),
        }
    }

    /// Canonical workspace root (resolved lazily, cached).
    pub async fn root(&self) -> Result<String, CodingError> {
        let mut cached = self.root.lock().await;
        if let Some(root) = &*cached {
            return root.clone();
        }
        let root = canonical_root(&self.workspace_root).await;
        // A failure (workspace not created yet, transient IO) is not cached:
        // the next call retries.
        if let Ok(resolved) = &root {
            *cached = Some(Ok(resolved.clone()));
        }
        root
    }

    /// Private scratch directory outside the workspace (created lazily).
    pub async fn scratch(&self) -> Result<String, CodingError> {
        let mut cached = self.scratch.lock().await;
        if let Some(scratch) = &*cached {
            return scratch.clone();
        }
        let created = create_scratch().await;
        *cached = Some(created.clone());
        created
    }

    pub async fn dispose(&self) {
        self.close();
        let dir = {
            let mut cached = self.scratch.lock().await;

            cached.take()
        };
        if let Some(Ok(dir)) = dir {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
    }

    pub(crate) fn close(&self) {
        self.closed.cancel();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub(crate) async fn closed(&self) {
        self.closed.cancelled().await;
    }
}

// The cached root/scratch cells live beside the immutable configuration.

async fn create_scratch() -> Result<String, CodingError> {
    let base = std::env::temp_dir();
    let base = tokio::fs::canonicalize(&base)
        .await
        .unwrap_or(base)
        .to_string_lossy()
        .into_owned();
    for _ in 0..64 {
        let candidate = format!("{base}/agentkit-coding-{}", uuid::Uuid::new_v4().simple());
        match create_private_dir(&candidate).await {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(CodingError::new("io_error", error.to_string()));
            }
        }
    }
    Err(CodingError::new(
        "io_error",
        "could not create a scratch directory",
    ))
}

/// Creates the directory with mode 0700 from the start, so it is never
/// briefly visible with the umask-derived mode.
async fn create_private_dir(path: &str) -> std::io::Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)
    })
    .await
    .map_err(|error| std::io::Error::other(error.to_string()))?
}

/// What a tool body returns; `define_tool` shapes it into a `ToolResult`.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub ok: bool,
    /// The text the model sees.
    pub text: String,
    pub summary: Option<String>,
    pub truncated: bool,
    pub warnings: Vec<String>,
    /// Structured details for UIs; the model sees `text`.
    pub details: Option<Value>,
}

impl Outcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            ok: true,
            text: text.into(),
            summary: None,
            truncated: false,
            warnings: vec![],
            details: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEffect {
    Read,
    Write,
}

impl ToolEffect {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

pub struct ToolSpec {
    pub name: &'static str,
    pub effect: ToolEffect,
    pub capability: &'static str,
    pub description: String,
    pub input_schema: Value,
    pub timeout_ms: u64,
}

pub fn failure(error: &CodingError) -> Outcome {
    let text = format!("{}: {}", error.code, error.message);
    Outcome {
        ok: false,
        summary: Some(text.clone()),
        text,
        truncated: false,
        warnings: vec![],
        details: None,
    }
}

/// Map an unexpected failure the way `toCodingError` does: an aborted call
/// reports `aborted`; anything else is `io_error` with a bounded message.
pub fn to_coding_error(message: impl Into<String>, cancelled: bool) -> CodingError {
    if cancelled {
        return CodingError::new("aborted", "the call was aborted");
    }
    let message: String = message.into();
    CodingError::new("io_error", message.chars().take(500).collect::<String>())
}

type BoxBody = Arc<dyn Fn(ToolContext, Value) -> BoxFuture<'static, Outcome> + Send + Sync>;

struct WrappedTool {
    definition: ToolDefinition,
    body: BoxBody,
    /// The tool's real execution environment (workspace root, sandbox and
    /// network policy, runner selection): the durable receipt binds to it
    /// so a recorded result replays only for the SAME environment. `None`
    /// keeps the historical empty-object binding.
    effect_context: Option<Value>,
}

#[async_trait]
impl Tool for WrappedTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn effect_context(&self) -> Option<Value> {
        self.effect_context.clone()
    }

    fn records_command_start(&self) -> bool {
        self.effect_context.is_some()
            && self.definition.capability == "process.exec"
            && matches!(self.definition.name.as_str(), "run_command" | "run_tests")
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let outcome = (self.body)(context.clone(), input).await;
        Ok(ToolResult {
            ok: outcome.ok,
            data: Some(
                outcome
                    .details
                    .clone()
                    .unwrap_or(Value::String(outcome.text.clone())),
            ),
            model_data: Some(Value::String(outcome.text)),
            sources: vec![],
            warnings: outcome.warnings,
            truncated: outcome.truncated,
            limits: context.limits.clone(),
            summary: outcome.summary,
            status: None,

            model_content: None,
        })
    }
}

/// Wraps a tool body so user errors become `ok:false` results and never
/// throw. Bodies return `Result<Outcome, CodingError>`; the error branch is
/// shaped exactly like the TypeScript `failure()`.
pub fn define_tool<F, Fut>(spec: ToolSpec, run: F) -> Arc<dyn Tool>
where
    F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Outcome, CodingError>> + Send + 'static,
{
    let definition = ToolDefinition {
        name: spec.name.to_owned(),
        version: "1".to_owned(),
        effect: spec.effect.as_str().to_owned(),
        capability: spec.capability.to_owned(),
        description: spec.description,
        input_schema: spec.input_schema,
        output_schema: None,
        timeout_ms: Some(spec.timeout_ms),
    };
    let body: BoxBody = Arc::new(move |context: ToolContext, input: Value| {
        let pending = run(context, input);
        Box::pin(async move {
            match pending.await {
                Ok(outcome) => outcome,
                Err(error) => failure(&error),
            }
        })
    });
    Arc::new(WrappedTool {
        definition,
        body,
        effect_context: None,
    })
}

/// As `define_tool`, but the tool declares the real environment its effect
/// happens in (workspace root, sandbox/network policy, runner selection).
/// The durable execution fingerprint binds to it, so a completed result is
/// replayed only under an identical environment and an environment change
/// is evidence, not a silent miss.
pub fn define_tool_with_effect_context<F, Fut>(
    spec: ToolSpec,
    effect_context: Value,
    run: F,
) -> Arc<dyn Tool>
where
    F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Outcome, CodingError>> + Send + 'static,
{
    let definition = ToolDefinition {
        name: spec.name.to_owned(),
        version: "1".to_owned(),
        effect: spec.effect.as_str().to_owned(),
        capability: spec.capability.to_owned(),
        description: spec.description,
        input_schema: spec.input_schema,
        output_schema: None,
        timeout_ms: Some(spec.timeout_ms),
    };
    let body: BoxBody = Arc::new(move |context: ToolContext, input: Value| {
        let pending = run(context, input);
        Box::pin(async move {
            match pending.await {
                Ok(outcome) => outcome,
                Err(error) => failure(&error),
            }
        })
    });
    Arc::new(WrappedTool {
        definition,
        body,
        effect_context: Some(effect_context),
    })
}
