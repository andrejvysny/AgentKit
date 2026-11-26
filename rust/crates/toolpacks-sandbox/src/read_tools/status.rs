//! `sandbox_get_status`: needs no machine and starts none. A fenced
//! workspace answers its status with `sandbox_fenced` named in it (ADR 0038
//! leaves the presentation to the pack); every other tool is refused there.

use agentkit_core::{ToolContext, ToolLimits, ToolResult};
use agentkit_workspaces::{WorkspaceState, WorkspaceStatus};
use serde_json::{Map, Value, json};

use crate::envelope::{failed, ok};
use crate::pack::SandboxEnvironment;
use crate::tool::Staged;

use super::refused;

/// `sandbox_get_status`.
pub(crate) async fn status(staged: &Staged, context: &ToolContext) -> ToolResult {
    let limits = &context.limits;
    let binding = match staged.attached().await {
        Ok(binding) => binding,
        Err(refusal) => return refused(&refusal, limits),
    };
    let service = &staged.shared.service;
    match service.status(&binding.binding, &binding.agent_id).await {
        Ok(status) => report(&status, &staged.shared.environment, limits),
        Err(error) => failed(error.code(), &error.to_string(), json!({}), limits),
    }
}

fn state_line(status: &WorkspaceStatus) -> String {
    match status.workspace.as_ref().map(|workspace| &workspace.state) {
        None => "not created yet: it starts at the first command or file write".to_owned(),
        Some(WorkspaceState::Ready) => "ready".to_owned(),
        Some(WorkspaceState::Busy) => "busy".to_owned(),
        Some(WorkspaceState::Fenced { reason, .. }) => format!(
            "fenced ({reason})\nsandbox_fenced: nothing runs in this sandbox until the owner recovers it"
        ),
        Some(WorkspaceState::Deleted) => "deleted".to_owned(),
    }
}

fn lines(status: &WorkspaceStatus, environment: &SandboxEnvironment) -> Vec<String> {
    let mut lines = vec![
        format!("Sandbox: {}", state_line(status)),
        format!("Image: {} ({})", environment.image_version, environment.os),
        format!("Working folder: {}", status.working_dir),
    ];
    if let Some(storage) = &status.storage {
        lines.push(format!(
            "Disk: {} of {} bytes used",
            storage.used_bytes, storage.quota_bytes
        ));
    }
    let hosts: Vec<String> = status.grants.iter().map(|g| g.target.to_string()).collect();
    lines.push(format!(
        "Allowed hosts: {}",
        if hosts.is_empty() {
            "none".to_owned()
        } else {
            hosts.join(", ")
        }
    ));
    for request in &status.pending_requests {
        lines.push(format!(
            "Waiting for the owner: {} ({} attempts)",
            request.target, request.count
        ));
    }
    lines.push(match &status.running {
        None => "Running: nothing".to_owned(),
        Some(running) => format!(
            "Running: {} (started by {}, run {})",
            running.operation.summary, running.operation.actor_agent_id, running.operation.run_id
        ),
    });
    lines
}

/// The network part of the structured status.
fn network_data(status: &WorkspaceStatus, data: &mut Map<String, Value>) {
    let hosts: Vec<Value> = status
        .grants
        .iter()
        .map(|g| json!({"host": g.target.host(), "port": g.target.port()}))
        .collect();
    data.insert("allowedHosts".into(), Value::Array(hosts));
    let pending: Vec<Value> = status
        .pending_requests
        .iter()
        .map(|r| json!({"requestId": r.id.as_str(), "host": r.target.host(), "port": r.target.port(), "attempts": r.count}))
        .collect();
    data.insert("pendingRequests".into(), Value::Array(pending));
}

fn report(
    status: &WorkspaceStatus,
    environment: &SandboxEnvironment,
    limits: &ToolLimits,
) -> ToolResult {
    let mut data = Map::new();
    let workspace = status.workspace.as_ref();
    let state = match workspace {
        None => json!("not_created"),
        Some(workspace) => serde_json::to_value(&workspace.state).unwrap_or(Value::Null),
    };
    data.insert("state".into(), state);
    data.insert(
        "workspaceId".into(),
        json!(workspace.map(|w| w.id.as_str())),
    );
    data.insert(
        "generation".into(),
        json!(workspace.map(|w| w.generation.0)),
    );
    data.insert("imageVersion".into(), json!(environment.image_version));
    data.insert("os".into(), json!(environment.os));
    data.insert("workingDir".into(), json!(status.working_dir));
    let storage = serde_json::to_value(status.storage).unwrap_or(Value::Null);
    data.insert("storage".into(), storage);
    network_data(status, &mut data);
    let running = status.running.as_ref().map(|running| {
        json!({
            "operationId": running.operation.id.as_str(),
            "summary": running.operation.summary,
            "agentId": running.operation.actor_agent_id.as_str(),
            "runId": running.operation.run_id,
            "holder": running.holder.as_ref().map(|holder| holder.label.clone()),
        })
    });
    data.insert("running".into(), json!(running));
    ok(lines(status, environment).join("\n"), data, limits)
}
