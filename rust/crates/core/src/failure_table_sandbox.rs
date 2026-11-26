//! Failure table part 4: the sandbox tool pack and the workspace service
//! (ADR 0038). The first rows are the spec's result-envelope table; the
//! rest are the codes the service and the pack added on the way.
use crate::failure_catalog::Spec;
use crate::failure_table::s;
use agentkit_contracts::AiToolOutcomeClass as C;

const INSPECT: &str = "Call sandbox_get_status to see the sandbox's state before deciding.";

pub(crate) static SANDBOX: &[Spec] = &[
    // The spec's result-envelope table.
    s(
        "sandbox_not_attached",
        C::Precondition,
        false,
        "No sandbox is attached to this run.",
        "Tell the user to switch the sandbox on for this chat or agent.",
    ),
    s(
        "sandbox_unavailable",
        C::Precondition,
        false,
        "The sandbox cannot start right now.",
        "Report the reason to the user; do not retry in a loop.",
    ),
    s(
        "sandbox_busy",
        C::Precondition,
        true,
        "Another operation holds the sandbox.",
        "Try again later; the message says who holds it.",
    ),
    s(
        "sandbox_fenced",
        C::Precondition,
        false,
        "The sandbox is fenced after an operation with an unknown outcome.",
        "Stop using the sandbox; the owner must recover it.",
    ),
    s(
        "network_denied",
        C::GuardRefused,
        false,
        "A connection was refused by the sandbox's network policy.",
        "Wait for the owner's decision on the recorded request before retrying.",
    ),
    s(
        "quota_exceeded",
        C::Precondition,
        false,
        "The sandbox's disk quota is reached.",
        "Free space in the sandbox, or ask the owner for more.",
    ),
    s(
        "path_outside_workspace",
        C::InvalidValue,
        true,
        "That path is outside the sandbox workspace.",
        "Use a path relative to the working folder, or under /workspace or ~/.",
    ),
    s(
        "not_a_regular_file",
        C::InvalidValue,
        true,
        "That path is a directory, a link or a special file.",
        "Call sandbox_list_files to find the regular file you meant.",
    ),
    // Codes the workspace service added.
    s(
        "sandbox_operation_conflict",
        C::Precondition,
        false,
        "This operation was already dispatched with different arguments.",
        INSPECT,
    ),
    s(
        "sandbox_stale_generation",
        C::Precondition,
        true,
        "The sandbox machine was replaced while the call ran.",
        "Call the tool again; it runs on the current machine.",
    ),
    s(
        "sandbox_not_found",
        C::InvalidValue,
        true,
        "That file or record does not exist in the sandbox.",
        "Call sandbox_list_files to find the right path.",
    ),
    s(
        "sandbox_cancelled",
        C::ToolError,
        false,
        "The call was cancelled before the operation started; nothing ran.",
        "Call it again only if the user still wants it.",
    ),
    s(
        "sandbox_backend_failed",
        C::ToolError,
        false,
        "The sandbox backend failed.",
        "Report the error to the user; do not retry in a loop.",
    ),
    s(
        "sandbox_store_failed",
        C::ToolError,
        false,
        "The sandbox's records could not be read or written.",
        "Report the error to the user; do not retry in a loop.",
    ),
    // Codes the tool pack added.
    s(
        "durable_execution_required",
        C::Precondition,
        false,
        "This sandbox tool runs only under the durable execution ledger, and this host has none.",
        "Do not repeat the call; tell the user this host cannot change the sandbox.",
    ),
    s(
        "plan_mode",
        C::GuardRefused,
        false,
        "This tool changes the sandbox and the run is in plan mode.",
        "Describe the change in your plan and wait for the user to approve it.",
    ),
    s(
        "permission_unavailable",
        C::Precondition,
        false,
        "The session's permission mode could not be read.",
        "Do not repeat the call; tell the user.",
    ),
    s(
        "write_failed",
        C::ToolError,
        false,
        "The file was not written.",
        INSPECT,
    ),
    s(
        "unknown_sandbox_tool",
        C::UnknownTool,
        false,
        "That name is not a sandbox tool.",
        "Use one of the sandbox tools in the sandbox toolset.",
    ),
    // Exports and the two boundary approvals.
    s(
        "export_approval_required",
        C::GuardRefused,
        false,
        "This export needs the user's approval of the file's content.",
        "Wait for the user's answer; do not repeat the call.",
    ),
    s(
        "network_approval_required",
        C::GuardRefused,
        false,
        "New network hosts need the user's approval.",
        "Wait for the user's answer; do not repeat the call.",
    ),
    s(
        "export_not_approved",
        C::Precondition,
        true,
        "This export call carries no approval of the file's current content.",
        "Call sandbox_export_file again so the user can approve the current content.",
    ),
    s(
        "export_stale",
        C::Precondition,
        true,
        "The file changed after it was shown for approval; nothing was exported.",
        "Call sandbox_export_file again to ask about the current content.",
    ),
    s(
        "export_refused",
        C::GuardRefused,
        false,
        "The host refused this export.",
        "Do not repeat the export; tell the user why it was refused.",
    ),
    s(
        "export_failed",
        C::ToolError,
        false,
        "The host could not store the export.",
        "Report the error to the user; do not retry in a loop.",
    ),
];
