//! `sandbox_run` and `sandbox_write_file`: the two tools whose effect is a
//! sandbox operation. The operation id is the ledger's execution id; the
//! workspace service writes the operation row before it dispatches and
//! never dispatches an id twice (INV-1, INV-3).

use agentkit_core::{ToolContext, ToolLimits, ToolResult};
use agentkit_workspaces::{ExecRequest, FileMode, OutputLimit, SandboxError, Sha256, WriteRequest};
use serde_json::{Value, json};

use crate::defs::ToolKind;
use crate::envelope::{Refusal, failed, not_started};
use crate::mutate::{admit, approve_shown};
use crate::paths::{Roots, file_ref, guest_path};
use crate::render;
use crate::tool::Staged;

fn text_arg<'a>(input: &'a Value, name: &str) -> Result<&'a str, Refusal> {
    input[name]
        .as_str()
        .ok_or_else(|| Refusal::new("invalid_arguments", format!("{name} must be a string")))
}

/// A refusal of the service. Every one comes before the operation's row:
/// the ledger hands each body an execution id no earlier delivery used, so
/// no row of it can exist yet. A conflict says otherwise; it is never
/// reported as not started.
fn service_refusal(error: SandboxError, limits: &ToolLimits) -> ToolResult {
    if let SandboxError::OperationConflict(_) = &error {
        return failed(error.code(), &error.to_string(), json!({}), limits);
    }
    not_started(&Refusal::from(error), limits)
}

fn exec_request(staged: &Staged, input: &Value) -> Result<ExecRequest, Refusal> {
    let command = text_arg(input, "command")?;
    let folder = staged.binding.binding.folder.as_ref();
    let directory = match input["directory"].as_str() {
        Some(text) => Some(file_ref(text, folder, Roots::Workspace)?.path),
        None => None,
    };
    Ok(ExecRequest {
        command: command.to_owned(),
        directory,
        timeout_ms: input["timeout"].as_u64(),
        output_limit: OutputLimit::LOG,
    })
}

/// `sandbox_run`.
pub(crate) async fn run(staged: &Staged, context: &ToolContext, input: &Value) -> ToolResult {
    let limits = &context.limits;
    let request = match exec_request(staged, input) {
        Ok(request) => request,
        Err(refusal) => return not_started(&refusal, limits),
    };
    let admitted = match admit(staged, ToolKind::Run, context, input).await {
        Ok(admitted) => admitted,
        Err(refusal) => return not_started(&refusal, limits),
    };
    let warnings = approve_shown(staged, ToolKind::Run, context, &admitted).await;
    let op = match admitted.op_context(context) {
        Ok(op) => op,
        Err(refusal) => return not_started(&refusal, limits),
    };
    let service = &staged.shared.service;
    let mut result = match service.exec(&op, &admitted.binding.binding, request).await {
        Ok(outcome) => render::command(&outcome, limits),
        Err(error) => service_refusal(error, limits),
    };
    result.warnings.extend(warnings);
    result
}

/// `sandbox_write_file`.
pub(crate) async fn write(staged: &Staged, context: &ToolContext, input: &Value) -> ToolResult {
    let limits = &context.limits;
    let folder = staged.binding.binding.folder.as_ref();
    let parsed = text_arg(input, "file_path")
        .and_then(|text| file_ref(text, folder, Roots::Both).map_err(Refusal::from))
        .and_then(|file| Ok((file, text_arg(input, "content")?)));
    let (file, content) = match parsed {
        Ok(parsed) => parsed,
        Err(refusal) => return not_started(&refusal, limits),
    };
    let admitted = match admit(staged, ToolKind::WriteFile, context, input).await {
        Ok(admitted) => admitted,
        Err(refusal) => return not_started(&refusal, limits),
    };
    let warnings = approve_shown(staged, ToolKind::WriteFile, context, &admitted).await;
    let op = match admitted.op_context(context) {
        Ok(op) => op,
        Err(refusal) => return not_started(&refusal, limits),
    };
    let written = render::Written {
        path: guest_path(folder, &file),
        bytes: content.len() as u64,
        sha256: Sha256::of(content.as_bytes()),
    };
    let request = WriteRequest {
        file,
        bytes: content.as_bytes().to_vec(),
        mode: FileMode::REGULAR,
    };
    let service = &staged.shared.service;
    let mut result = match service
        .write_file(&op, &admitted.binding.binding, request)
        .await
    {
        Ok(outcome) => render::write(&outcome, &written, limits),
        Err(error) => service_refusal(error, limits),
    };
    result.warnings.extend(warnings);
    result
}
