//! `SandboxPermissionGuard`: the sandbox mode table and its two approvals.
//!
//! | Mode | read, list, status | run, write | export | new host |
//! |---|---|---|---|---|
//! | Read-only | allow | refuse | refuse | not reachable |
//! | Plan | allow | refuse, final | refuse, final | not reachable |
//! | Ask before changes | allow | allow | approval | approval |
//! | Edit automatically | allow | allow | approval | approval |
//! | Full access | allow | allow | allow (D-29) | approval |
//!
//! An export asks with its path, title, size, type, SHA-256 and a text
//! preview: the model chose path, title and content, so they are escaped
//! (`card`) and the card's own lines come before the content. The guard
//! reads the whole file (up to 64 MiB) on every evaluation; no hash is
//! cached, since a cache keyed by size and mtime could show a stale card. A
//! command or write of a run whose workspace has pending network requests
//! asks "Allow connections to host:port from this sandbox." for each one
//! not shown to the run before. What was shown is recorded in the pack, and
//! the body acts on exactly that (see `asks`). A stored ask of the same
//! call shape is re-presented verbatim, so the same call re-asking parks
//! the same request content.

use std::sync::Arc;

use agentkit_governance::{
    ApprovalAsk, GovernanceResult, ToolGuard, ToolGuardContext, ToolGuardVerdict, args_sha256,
};
use agentkit_workspaces::{ExportFile, NetworkRequest};
use async_trait::async_trait;
use serde_json::Value;

use crate::SANDBOX_NAMESPACE;
use crate::asks::{Ask, AskKey, NewAsk, Shown};
use crate::card;
use crate::defs::{EXPORT_FILE, ToolKind};
use crate::export_tool::export_title;
use crate::mode::{SandboxMode, effective_mode};
use crate::pack::Shared;
use crate::paths::{Roots, file_ref, guest_path};
use crate::ports::SandboxBinding;

const PREVIEW_LINES: usize = 20;
const PREVIEW_CHARS: usize = 1_000;
/// Longest escaped path a card shows; a longer one is refused, so a stored
/// ask stays small.
const CARD_PATH_MAX_CHARS: usize = 1_024;

/// The pack's permission guard. Judges only the `sandbox` namespace; guards
/// compose with AND and a refusal anywhere wins.
pub struct SandboxPermissionGuard {
    shared: Arc<Shared>,
}

fn awaiting(ask: &Ask) -> ToolGuardVerdict {
    ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
        reason: ask.reason.clone(),
        preview: Some(ask.preview.clone()),
        requested_mode: None,
        uncertainty_generation: None,
    })
}

fn refusal_text(code: &str, message: &str) -> String {
    format!("{code}: {message}")
}

/// The export card. `path` and `title` are already escaped.
fn export_preview(path: &str, title: &str, file: &ExportFile) -> String {
    let text = std::str::from_utf8(&file.bytes)
        .ok()
        .filter(|text| !text.contains('\0'));
    let mut preview = format!(
        "Export this file from the sandbox to the user?\nPath: {path}\nTitle: {title}\nSize: {} bytes\nType: {}\nSHA-256: {}",
        file.size,
        if text.is_some() { "text" } else { "binary" },
        file.sha256
    );
    if let Some(text) = text {
        preview.push_str("\nPreview:\n");
        preview.push_str(&card::preview(text, PREVIEW_LINES, PREVIEW_CHARS));
    }
    preview
}

/// The ask about exporting `current` from `path` under the call's title,
/// or why the call is refused before anything is asked.
fn new_export_ask(input: &Value, path: &str, current: &ExportFile) -> Result<NewAsk, String> {
    let title = export_title(input, path).map_err(|r| refusal_text(&r.code, &r.message))?;
    let shown_path = card::field(path);
    let chars = shown_path.chars().count();
    if chars > CARD_PATH_MAX_CHARS {
        let message = format!(
            "the path is {chars} characters as shown on an approval card, more than {CARD_PATH_MAX_CHARS}; copy the file to a shorter path and export that"
        );
        return Err(refusal_text("invalid_arguments", &message));
    }
    Ok(NewAsk {
        reason: refusal_text("export_approval_required", &shown_path),
        preview: export_preview(&shown_path, &card::field(&title), current),
        shown: Shown::Export {
            path: path.to_owned(),
            sha256: current.sha256.clone(),
        },
    })
}

fn network_preview(requests: &[&NetworkRequest]) -> String {
    requests
        .iter()
        .map(|request| {
            let attempts = if request.count == 1 {
                "attempt"
            } else {
                "attempts"
            };
            format!(
                "Allow connections to {} from this sandbox. ({} {attempts})",
                request.target, request.count
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl SandboxPermissionGuard {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// The file an export names and its content now; `None` when it
    /// cannot be read (the body then reports why, and exports nothing
    /// without an ask).
    async fn export_target(
        &self,
        context: &ToolGuardContext,
        binding: &SandboxBinding,
    ) -> Option<(String, ExportFile)> {
        let input = context.arguments.as_ref()?;
        let folder = binding.binding.folder.as_ref();
        let file = file_ref(input["file_path"].as_str()?, folder, Roots::Both).ok()?;
        let service = &self.shared.service;
        let read = service.read_for_export(&binding.binding, &binding.agent_id, &file);
        Some((guest_path(folder, &file), read.await.ok()?))
    }

    /// Ask about the exact content of the file, or re-present the stored
    /// ask; a file changed since it was shown is refused as stale.
    async fn export_ask(
        &self,
        context: &ToolGuardContext,
        binding: &SandboxBinding,
        args: String,
    ) -> GovernanceResult<ToolGuardVerdict> {
        let Some((path, current)) = self.export_target(context, binding).await else {
            return Ok(ToolGuardVerdict::Allowed);
        };
        let input = context.arguments.as_ref().unwrap_or(&Value::Null);
        let run = &context.contribution.run_id;
        let key = AskKey {
            tool: EXPORT_FILE,
            args_sha256: args,
        };
        let new = match new_export_ask(input, &path, &current) {
            Ok(new) => new,
            Err(reason) => return Ok(ToolGuardVerdict::Refused(reason)),
        };
        let asks = &self.shared.asks;
        if let Some(ask) = asks.stored(run, &key) {
            if ask.shown == new.shown {
                return Ok(awaiting(&ask));
            }
            asks.forget(run, &key);
            let message = format!(
                "{path} changed after it was shown for approval; nothing was exported. Call {EXPORT_FILE} again to ask about the current content (SHA-256 {})",
                current.sha256
            );
            return Ok(ToolGuardVerdict::Refused(refusal_text(
                "export_stale",
                &message,
            )));
        }
        let ask = asks.raise(run, key, new, context.contribution.durable.clone());
        Ok(awaiting(&ask))
    }

    /// The pending requests of the attached workspace; none without one.
    async fn pending(&self, binding: &SandboxBinding) -> Vec<NetworkRequest> {
        let service = &self.shared.service;
        let Ok(Some(workspace)) = service.lookup(&binding.binding).await else {
            return vec![];
        };
        service
            .pending_requests(&workspace.id)
            .await
            .unwrap_or_default()
    }

    /// Ask about the pending network requests not shown to this run yet,
    /// or re-present the stored ask of this call shape while any request in
    /// it is still pending. Without a workspace or readable requests
    /// nothing is asked and nothing will be granted.
    async fn network_ask(
        &self,
        context: &ToolGuardContext,
        binding: &SandboxBinding,
        kind: ToolKind,
        args: String,
    ) -> GovernanceResult<ToolGuardVerdict> {
        let pending = self.pending(binding).await;
        let run = &context.contribution.run_id;
        let key = AskKey {
            tool: kind.name(),
            args_sha256: args,
        };
        let asks = &self.shared.asks;
        if let Some(ask) = asks.stored(run, &key) {
            let live = matches!(&ask.shown, Shown::Network { requests }
                if requests.iter().any(|id| pending.iter().any(|p| &p.id == id)));
            if live {
                return Ok(awaiting(&ask));
            }
            asks.forget(run, &key);
        }
        let all = pending.iter().map(|request| request.id.clone()).collect();
        let ids = asks.unshown(run, all);
        let shown: Vec<&NetworkRequest> = pending.iter().filter(|r| ids.contains(&r.id)).collect();
        if shown.is_empty() {
            return Ok(ToolGuardVerdict::Allowed);
        }
        let targets: Vec<String> = shown.iter().map(|r| r.target.to_string()).collect();
        let new = NewAsk {
            reason: refusal_text("network_approval_required", &targets.join(", ")),
            preview: network_preview(&shown),
            shown: Shown::Network { requests: ids },
        };
        let ask = asks.raise(run, key, new, context.contribution.durable.clone());
        Ok(awaiting(&ask))
    }

    async fn mode(
        &self,
        context: &ToolGuardContext,
        binding: &SandboxBinding,
    ) -> GovernanceResult<SandboxMode> {
        effective_mode(
            self.shared.authority.as_ref(),
            &context.contribution,
            binding.mode,
        )
        .await
    }
}

#[async_trait]
impl ToolGuard for SandboxPermissionGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        if context.namespace != SANDBOX_NAMESPACE {
            return Ok(ToolGuardVerdict::Allowed);
        }
        let Some(kind) = ToolKind::from_name(&context.tool.name) else {
            let message = format!("{} is not a sandbox tool", context.tool.name);
            return Ok(ToolGuardVerdict::Refused(refusal_text(
                "unknown_sandbox_tool",
                &message,
            )));
        };
        if !kind.changes() {
            return Ok(ToolGuardVerdict::Allowed);
        }
        // No attachment: the body answers `sandbox_not_attached`.
        let Some(binding) = self.shared.resolver.binding(&context.contribution).await? else {
            return Ok(ToolGuardVerdict::Allowed);
        };
        let mode = self.mode(context, &binding).await?;
        if let Some((code, message)) = mode.refusal(kind) {
            return Ok(ToolGuardVerdict::Refused(refusal_text(code, &message)));
        }
        let Some(input) = context.arguments.as_ref() else {
            return Ok(ToolGuardVerdict::Allowed);
        };
        let args = args_sha256(input);
        match kind {
            ToolKind::ExportFile if mode.export_needs_approval() => {
                self.export_ask(context, &binding, args).await
            }
            ToolKind::ExportFile => Ok(ToolGuardVerdict::Allowed),
            _ => self.network_ask(context, &binding, kind, args).await,
        }
    }
}
