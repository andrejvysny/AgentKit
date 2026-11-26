//! `sandbox_export_file`: a boundary crossing. Outside Full access the
//! owner approves the exact content first (F-14, SBX-08): the body reads the
//! file once, and exports those bytes only when their SHA-256 and path are
//! the ones the guard showed in an ask raised before this attempt, and this
//! call carries the owner's one-use answer for these arguments. A changed
//! file, a lost ask (restart) or a missing answer is refused and nothing is
//! exported. No standing approval covers an export, and neither does an
//! answer to an uncertain re-run: its card leads with the uncertainty and
//! lists the file below it, but that yes approves the re-run only, never
//! the content, so the body refuses it and the model asks again.

use agentkit_core::{ToolContext, ToolLimits, ToolResult};
use agentkit_governance::ApprovalGrant;
use agentkit_workspaces::{ExportFile, FileRef, Sha256};
use serde_json::{Map, Value, json};

use crate::asks::{AskKey, Shown, Taken};
use crate::defs::{EXPORT_FILE, TITLE_MAX_CHARS, ToolKind};
use crate::envelope::{Refusal, failed, ok};
use crate::mutate::{Admitted, admit, grant};
use crate::paths::{Roots, file_ref, guest_path};
use crate::ports::ExportContext;
use crate::tool::Staged;

fn refused(refusal: &Refusal, limits: &ToolLimits) -> ToolResult {
    failed(&refusal.code, &refusal.message, json!({}), limits)
}

/// What a body checks an export against: the answer the call carries, the
/// ask it answers, the file just read.
struct Approval<'a> {
    grant: Option<&'a ApprovalGrant>,
    taken: Option<&'a Taken>,
    call_id: &'a str,
    path: &'a str,
    current: &'a Sha256,
}

/// Why the bytes just read may not leave, if they may not.
fn unapproved(approval: &Approval<'_>) -> Option<Refusal> {
    let (path, current) = (approval.path, approval.current);
    let Some(grant) = approval.grant else {
        return Some(Refusal::new(
            "export_not_approved",
            format!(
                "this call carries no answer of the user approving {path}; call {EXPORT_FILE} again so the user can approve its current content"
            ),
        ));
    };
    if grant.uncertainty_generation.is_some() {
        return Some(Refusal::new(
            "export_not_approved",
            format!(
                "the user approved running an uncertain earlier export again, not the content of {path}; nothing was exported. Call {EXPORT_FILE} again so the user can approve its current content"
            ),
        ));
    }
    let shown = approval
        .taken
        .filter(|taken| taken.answered_after_ask(approval.call_id, grant))
        .map(|taken| &taken.ask.shown);
    match shown {
        Some(Shown::Export {
            path: shown,
            sha256,
        }) if shown == path && sha256 == current => None,
        Some(Shown::Export { sha256, .. }) => Some(Refusal::new(
            "export_stale",
            format!(
                "{path} changed after the user approved it (approved SHA-256 {sha256}, now {current}); nothing was exported. Call {EXPORT_FILE} again to ask about the current content"
            ),
        )),
        _ => Some(Refusal::new(
            "export_stale",
            format!(
                "the approval of {path} cannot be matched to the content the user saw; nothing was exported. Call {EXPORT_FILE} again to ask about the current content"
            ),
        )),
    }
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// The title the export is delivered under (and the card shows): the
/// model's `title`, else the file name of `path`. One too long to show is
/// refused.
pub(crate) fn export_title(input: &Value, path: &str) -> Result<String, Refusal> {
    let Some(title) = input["title"].as_str() else {
        return Ok(file_name(path));
    };
    let chars = title.chars().count();
    if chars as u64 > TITLE_MAX_CHARS {
        return Err(Refusal::new(
            "invalid_arguments",
            format!("title is {chars} characters; at most {TITLE_MAX_CHARS} are allowed"),
        ));
    }
    Ok(title.to_owned())
}

/// An export call past its prelude.
struct Prepared {
    file: FileRef,
    /// The absolute guest path, as the card shows it.
    path: String,
    title: String,
    admitted: Admitted,
}

/// The prelude of an export: the file named, its title, and the call
/// admitted.
async fn prepare(
    staged: &Staged,
    context: &ToolContext,
    input: &Value,
) -> Result<Prepared, Refusal> {
    let folder = staged.binding.binding.folder.as_ref();
    let text = input["file_path"]
        .as_str()
        .ok_or_else(|| Refusal::new("invalid_arguments", "file_path must be a string"))?;
    let file = file_ref(text, folder, Roots::Both)?;
    let path = guest_path(folder, &file);
    let title = export_title(input, &path)?;
    let admitted = admit(staged, ToolKind::ExportFile, context, input).await?;
    Ok(Prepared {
        file,
        path,
        title,
        admitted,
    })
}

/// `sandbox_export_file`.
pub(crate) async fn export(staged: &Staged, context: &ToolContext, input: &Value) -> ToolResult {
    let limits = &context.limits;
    let Prepared {
        file,
        path,
        title,
        admitted,
    } = match prepare(staged, context, input).await {
        Ok(prepared) => prepared,
        Err(refusal) => return refused(&refusal, limits),
    };
    // Single use: taken before the read, whatever the read answers.
    let key = AskKey {
        tool: EXPORT_FILE,
        args_sha256: admitted.args_sha256.clone(),
    };
    let run = &staged.contribution.run_id;
    let taken = staged.shared.asks.take(run, &key, staged.staged);
    let service = &staged.shared.service;
    let read = service.read_for_export(&admitted.binding.binding, admitted.agent(), &file);
    let exported = match read.await {
        Ok(exported) => exported,
        Err(error) => {
            return failed(
                error.code(),
                &error.to_string(),
                json!({"path": path}),
                limits,
            );
        }
    };
    if admitted.mode.export_needs_approval() {
        let grant = grant(staged, context, &admitted.args_sha256);
        let approval = Approval {
            grant: grant.as_ref(),
            taken: taken.as_ref(),
            call_id: context.tool_call_id.as_deref().unwrap_or_default(),
            path: &path,
            current: &exported.sha256,
        };
        if let Some(refusal) = unapproved(&approval) {
            let extra = json!({"path": path, "sha256": exported.sha256.as_str()});
            return failed(&refusal.code, &refusal.message, extra, limits);
        }
    }
    deliver(staged, context, &admitted, path, exported, &title).await
}

async fn deliver(
    staged: &Staged,
    context: &ToolContext,
    admitted: &Admitted,
    path: String,
    exported: ExportFile,
    title: &str,
) -> ToolResult {
    let limits = &context.limits;
    let (size, sha256) = (exported.size, exported.sha256.clone());
    let export = ExportContext {
        execution_id: admitted.execution_id.clone(),
        run_id: context.run_id.clone(),
        chat_id: context.chat_id.clone(),
        tool_call_id: context.tool_call_id.clone(),
        agent_id: admitted.agent().clone(),
        workspace_key: admitted.binding.binding.key.clone(),
        path: path.clone(),
    };
    let mut data = Map::new();
    data.insert("path".into(), json!(path));
    data.insert("size".into(), json!(size));
    data.insert("sha256".into(), json!(sha256.as_str()));
    data.insert("title".into(), json!(title));
    match staged.shared.sink.export(&export, exported, title).await {
        Ok(receipt) => {
            data.insert("fileId".into(), json!(receipt.file_id));
            data.insert("kind".into(), json!(receipt.kind.as_str()));
            let text = format!(
                "Exported {path} ({size} bytes, SHA-256 {sha256}) to the user as {} {} \"{title}\".",
                receipt.kind.as_str(),
                receipt.file_id
            );
            ok(text, data, limits)
        }
        Err(error) => failed(
            error.code(),
            &error.to_string(),
            Value::Object(data),
            limits,
        ),
    }
}
