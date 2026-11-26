//! One staged sandbox tool: its definition, the effect context it was
//! staged with, and the body of its kind. A body always answers a result;
//! user-level errors are `ok: false` envelopes, never `Err`.

use std::sync::Arc;

use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolLoading, ToolResult};
use agentkit_governance::ToolContributionContext;
use async_trait::async_trait;
use serde_json::Value;

use crate::SANDBOX_TOOLSET;
use crate::defs::ToolKind;
use crate::envelope::Refusal;
use crate::mode::{SandboxMode, effective_mode};
use crate::pack::Shared;
use crate::ports::SandboxBinding;
use crate::{export_tool, read_tools, run_tool};

/// The tools of one staging (one run attempt) and what they were staged
/// with.
pub(crate) struct Staged {
    pub(crate) shared: Arc<Shared>,
    pub(crate) contribution: ToolContributionContext,
    /// The attachment the tools were offered for.
    pub(crate) binding: SandboxBinding,
    /// This staging's number: only asks created before it count.
    pub(crate) staged: u64,
}

impl Staged {
    /// The attachment again, at call time: the host may have detached the
    /// run or pinned another workspace since staging.
    pub(crate) async fn attached(&self) -> Result<SandboxBinding, Refusal> {
        let detached = || {
            Refusal::new(
                "sandbox_not_attached",
                "no sandbox is attached to this run any more; tell the user to switch the sandbox on for this chat or agent",
            )
        };
        match self.shared.resolver.binding(&self.contribution).await {
            Ok(Some(now))
                if now.binding == self.binding.binding && now.agent_id == self.binding.agent_id =>
            {
                Ok(now)
            }
            Ok(_) => Err(detached()),
            Err(error) => Err(Refusal::new(
                "sandbox_not_attached",
                format!("the host could not confirm this run's sandbox: {error}"),
            )),
        }
    }

    /// The mode the call runs under (an authority that cannot answer
    /// refuses the call).
    pub(crate) async fn mode(&self, binding: &SandboxBinding) -> Result<SandboxMode, Refusal> {
        effective_mode(
            self.shared.authority.as_ref(),
            &self.contribution,
            binding.mode,
        )
        .await
        .map_err(|error| {
            Refusal::new(
                "permission_unavailable",
                format!("the session's permission mode could not be read: {error}"),
            )
        })
    }
}

pub(crate) struct SandboxTool {
    kind: ToolKind,
    definition: ToolDefinition,
    staged: Arc<Staged>,
    effect_context: Option<Value>,
}

impl SandboxTool {
    pub(crate) fn new(kind: ToolKind, staged: Arc<Staged>, effect_context: Option<Value>) -> Self {
        Self {
            kind,
            definition: kind.definition(),
            staged,
            effect_context,
        }
    }
}

#[async_trait]
impl Tool for SandboxTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn effect_context(&self) -> Option<Value> {
        self.effect_context.clone()
    }

    /// Commands and writes distinguish a refusal before their operation row
    /// (not started) from an unknown outcome, as the ledger requires.
    fn records_command_start(&self) -> bool {
        self.kind.operates()
    }

    fn loading(&self) -> ToolLoading {
        ToolLoading::Deferred
    }

    fn toolset(&self) -> Option<&str> {
        Some(SANDBOX_TOOLSET)
    }

    fn arg_aliases(&self) -> Vec<(String, String)> {
        self.kind
            .aliases()
            .iter()
            .map(|(alias, canonical)| ((*alias).to_owned(), (*canonical).to_owned()))
            .collect()
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let staged = &self.staged;
        Ok(match self.kind {
            ToolKind::Run => run_tool::run(staged, &context, &input).await,
            ToolKind::WriteFile => run_tool::write(staged, &context, &input).await,
            ToolKind::ExportFile => export_tool::export(staged, &context, &input).await,
            ToolKind::ReadFile => read_tools::read_file(staged, &context, &input).await,
            ToolKind::ListFiles => read_tools::list_files(staged, &context, &input).await,
            ToolKind::GetStatus => read_tools::status(staged, &context).await,
        })
    }
}
