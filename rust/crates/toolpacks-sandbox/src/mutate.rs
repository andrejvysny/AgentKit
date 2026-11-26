//! What every write-effect body checks before it touches the sandbox, and
//! the network approval a command or write carries out first.
//!
//! Order: the admitted execution id (no ledger, no write: INV-2) -> the
//! attachment again -> the mode table (a refusal here holds even when the
//! host did not register the guard, SBX-17) -> the host's dispatch guard
//! (the task lease) -> then the body.

use agentkit_core::ToolContext;
use agentkit_governance::{ApprovalGrant, EXECUTION_ID_METADATA_KEY, args_sha256};
use agentkit_workspaces::{
    AgentId, NetworkDecision, OpContext, OperationId, RepositoryError, RequestId, SandboxError,
    Sha256,
};
use serde_json::Value;

use crate::asks::{AskKey, Shown};
use crate::defs::ToolKind;
use crate::envelope::Refusal;
use crate::mode::SandboxMode;
use crate::ports::SandboxBinding;
use crate::tool::Staged;

/// A write-effect call that passed the prelude.
pub(crate) struct Admitted {
    pub(crate) binding: SandboxBinding,
    pub(crate) mode: SandboxMode,
    /// The ledger's `execution_id`: the operation id (INV-2).
    pub(crate) execution_id: String,
    /// The canonical argument hash the ledger admitted the call under.
    pub(crate) args_sha256: String,
}

impl Admitted {
    /// The workspace service's view of this call.
    pub(crate) fn op_context(&self, context: &ToolContext) -> Result<OpContext, Refusal> {
        let args = Sha256::parse(&self.args_sha256)
            .map_err(|error| Refusal::new("invalid_arguments", error.to_string()))?;
        Ok(OpContext {
            operation_id: OperationId::from_execution_id(self.execution_id.clone()),
            args_sha256: args,
            run_id: context.run_id.clone(),
            agent_id: self.agent().clone(),
            cancellation: context.cancellation.clone(),
        })
    }

    pub(crate) fn agent(&self) -> &AgentId {
        &self.binding.agent_id
    }
}

/// Run the prelude for `kind`.
pub(crate) async fn admit(
    staged: &Staged,
    kind: ToolKind,
    context: &ToolContext,
    input: &Value,
) -> Result<Admitted, Refusal> {
    let execution_id = context
        .metadata
        .get(EXECUTION_ID_METADATA_KEY)
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            Refusal::new(
                "durable_execution_required",
                "this tool changes the sandbox and runs only after the durable execution ledger admitted the call; this host did not",
            )
        })?
        .to_owned();
    let binding = staged.attached().await?;
    let mode = staged.mode(&binding).await?;
    if let Some((code, message)) = mode.refusal(kind) {
        return Err(Refusal::new(code, message));
    }
    if let Some(guard) = &context.dispatch_guard {
        guard
            .check()
            .await
            .map_err(|error| Refusal::new(&error.code, error.message))?;
    }
    Ok(Admitted {
        binding,
        mode,
        execution_id,
        args_sha256: args_sha256(input),
    })
}

/// The one-use answer this call resumes, when it was given for exactly
/// these arguments.
pub(crate) fn grant(staged: &Staged, context: &ToolContext, args: &str) -> Option<ApprovalGrant> {
    let durable = staged.contribution.durable.as_ref()?;
    let call = context.tool_call_id.as_deref()?;
    durable
        .approval_grant(call)
        .filter(|grant| grant.args_sha256 == args)
}

/// Grant exactly the network requests the owner was shown for this call,
/// when this call carries the owner's answer to that card: given after the
/// card was raised, for these arguments, and not to an uncertain re-run
/// (its card leads with the uncertainty and lists the hosts below it, but
/// that yes approves the re-run only, never a host). The ask is taken
/// whatever happens (single use); requests it granted nothing for count as
/// not shown, so the run asks about them again. Returns warnings for
/// requests that could not be granted.
pub(crate) async fn approve_shown(
    staged: &Staged,
    kind: ToolKind,
    context: &ToolContext,
    admitted: &Admitted,
) -> Vec<String> {
    let key = AskKey {
        tool: kind.name(),
        args_sha256: admitted.args_sha256.clone(),
    };
    let run = &staged.contribution.run_id;
    let Some(taken) = staged.shared.asks.take(run, &key, staged.staged) else {
        return vec![];
    };
    let call = context.tool_call_id.as_deref().unwrap_or_default();
    let answered = grant(staged, context, &admitted.args_sha256).is_some_and(|grant| {
        grant.uncertainty_generation.is_none() && taken.answered_after_ask(call, &grant)
    });
    let Shown::Network { requests } = taken.ask.shown else {
        return vec![];
    };
    if !answered {
        staged.shared.asks.unshow(run, &requests);
        return vec![];
    }
    grant_requests(staged, admitted, &requests).await
}

/// Approve each of `requests` (pending requests of the attached workspace).
async fn grant_requests(
    staged: &Staged,
    admitted: &Admitted,
    requests: &[RequestId],
) -> Vec<String> {
    let service = &staged.shared.service;
    let Ok(Some(workspace)) = service.lookup(&admitted.binding.binding).await else {
        let warning =
            "the approved network hosts could not be granted: the workspace could not be read";
        return vec![warning.to_owned()];
    };
    let mut warnings = vec![];
    for id in requests {
        let decided = service
            .decide_request(&workspace.id, id, NetworkDecision::Approve)
            .await;
        match decided {
            // Decided meanwhile (by the owner elsewhere): their decision stands.
            Ok(_)
            | Err(SandboxError::Repository(
                RepositoryError::RequestNotFound(_) | RepositoryError::RequestNotPending { .. },
            )) => {}
            Err(error) => warnings.push(format!(
                "the approved network request {id} could not be granted: {error}"
            )),
        }
    }
    warnings
}
