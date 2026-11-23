//! The one table of tool failure codes (TU-10), part 1: the pipeline and guards.
//! Parts 2 and 3 are `failure_table_files.rs` and `failure_table_agents.rs`.
//! Every code a tool, guard or
//! pipeline stage can raise is listed here with its class, a default sentence
//! (used when only the bare code is known) and one concrete next step.
//! `tests/failure_catalog.rs` fails on a code used in source but absent here.
use crate::failure_catalog::Spec;
use agentkit_contracts::AiToolOutcomeClass as C;

pub(crate) const fn s(
    code: &'static str,
    class: C,
    retryable: bool,
    message: &'static str,
    next_step: &'static str,
) -> Spec {
    Spec {
        code,
        class,
        retryable,
        message,
        next_step,
    }
}

const GENERIC: &str = "Read the message, adjust the call, and try once more.";
const ASK: &str = "Do not repeat this call; choose another approach or ask the user.";

pub(crate) static CORE: &[Spec] = &[
    // Call pipeline and run loop.
    s(
        "tool_missing",
        C::UnknownTool,
        true,
        "That tool does not exist.",
        "Use one of the closest tool names listed, or call tool_search to find the right one.",
    ),
    s(
        "tool_name_missing",
        C::InvalidStructure,
        true,
        "The tool call had no name.",
        "Call a tool by its exact name; call tool_search if you are unsure which fits.",
    ),
    s(
        "tool_not_allowed",
        C::GuardRefused,
        false,
        "That tool is not available this turn.",
        "Use one of the tools listed as available this turn; call tool_search to see all of them.",
    ),
    s(
        "tool_unavailable",
        C::Precondition,
        true,
        "That tool is in this session's tool list but cannot run right now.",
        "Use another offered tool, or try this one again in a later turn.",
    ),
    s(
        "tool_renamed",
        C::Precondition,
        true,
        "That tool was renamed or retired.",
        "Call the replacement tool the message names instead.",
    ),
    s(
        "tool_not_found",
        C::UnknownTool,
        true,
        "No tool has that name.",
        "Use one of the closest names listed, or call tool_search to find the right tool.",
    ),
    s(
        "tool_deferred",
        C::Precondition,
        true,
        "That tool is deferred: it is not in your tool list.",
        "Call it through tool_call with its name and arguments; tool_describe shows its schema.",
    ),
    s(
        "invalid_arguments",
        C::InvalidStructure,
        true,
        "The arguments do not match the tool's schema.",
        "Fix the listed arguments to match the schema shown and call tool_call again.",
    ),
    s(
        "bad_args",
        C::InvalidJson,
        true,
        "The arguments were not valid JSON.",
        "Send the arguments as one JSON object and call the tool again.",
    ),
    s(
        "schema_invalid",
        C::InvalidStructure,
        true,
        "The arguments do not match the tool schema.",
        "Fix the listed arguments and call again.",
    ),
    s(
        "tool_hook_denied",
        C::GuardRefused,
        false,
        "A loop hook refused this call.",
        ASK,
    ),
    s(
        "tool_guard_refused",
        C::GuardRefused,
        false,
        "A guard refused this call.",
        ASK,
    ),
    s(
        "quoted_call",
        C::GuardRefused,
        false,
        crate::quoted_call::REFUSED,
        "If you were only showing that block, continue your answer and describe it in words instead of repeating its markup exactly. If the user wants this action carried out, tell them what it would do and ask them to confirm in a new message.",
    ),
    s(
        "tool_call_cap",
        C::ToolError,
        true,
        "This call was skipped because the step already had too many tool calls.",
        "Make fewer calls per step and repeat the skipped call in your next step.",
    ),
    s(
        "cancelled",
        C::ToolError,
        false,
        "The run was cancelled before this tool produced a result.",
        "Stop; do not retry this call.",
    ),
    s(
        "suspend_invalid",
        C::InvalidValue,
        true,
        "The tool asked the human a question without a prompt.",
        "Call again with a non-empty prompt for the human.",
    ),
    s(
        "suspend_conflict",
        C::Precondition,
        true,
        "Another call in this step is already waiting for the human.",
        "Wait for the human answer, then call again.",
    ),
    s(
        "suspension_unavailable",
        C::Precondition,
        false,
        "This run cannot wait for a human.",
        "Continue without asking; state the assumption you are making.",
    ),
    s(
        "exec_failed",
        C::ToolError,
        true,
        "The tool failed while running.",
        GENERIC,
    ),
    s(
        "tool_failed",
        C::ToolError,
        true,
        "The tool reported failure.",
        GENERIC,
    ),
    s(
        "tool_timeout",
        C::Timeout,
        true,
        "The tool timed out.",
        "Narrow the command, or pass a longer timeout.",
    ),
    s(
        "result_unserializable",
        C::ToolError,
        false,
        "The tool result could not be turned into JSON.",
        "Ask for a smaller or simpler result.",
    ),
    s(
        "execution_timeout",
        C::Timeout,
        false,
        "The run's time budget is spent.",
        "Finish with what you have and explain what is left.",
    ),
    s(
        "execution_budget_exhausted",
        C::Timeout,
        false,
        "The run's tool budget is spent.",
        "Finish with what you have and explain what is left.",
    ),
    s(
        "invalid_budget",
        C::InvalidValue,
        false,
        "The execution budget is invalid.",
        "Report this to the user; the budget must be fixed in the configuration.",
    ),
    s(
        "provider_error",
        C::ToolError,
        true,
        "The model provider failed.",
        "Retry once; if it fails again, report the error to the user.",
    ),
    s(
        "incomplete_stream",
        C::ToolError,
        true,
        "The model response ended early.",
        "Retry once; if it fails again, report the error to the user.",
    ),
    s(
        "output_truncated",
        C::ToolError,
        false,
        "The output was cut at the size limit.",
        "Narrow the request (smaller range, tighter pattern, lower limit) to see the rest.",
    ),
    // Guards and approvals.
    s(
        "approval_denied",
        C::ApprovalDenied,
        false,
        "The user denied this tool call.",
        "Do not repeat the call; ask the user what they would prefer.",
    ),
    s(
        "approval_required",
        C::GuardRefused,
        false,
        "This call needs the user's approval before it can run.",
        "Wait for the user's answer; do not repeat the call.",
    ),
    s(
        "plan_approval_required",
        C::GuardRefused,
        false,
        "The plan needs the user's approval before work continues.",
        "Wait for the user's answer; do not repeat the call.",
    ),
    s(
        "read_only",
        C::GuardRefused,
        false,
        "This tool changes things and the session is read-only.",
        "Describe the change instead of making it.",
    ),
    s(
        "role_ceiling",
        C::GuardRefused,
        false,
        "The current role may not use this kind of tool.",
        ASK,
    ),
    s(
        "role_read_only",
        C::GuardRefused,
        false,
        "This role is read-only.",
        "Describe the change instead of making it.",
    ),
    s(
        "unknown_capability",
        C::GuardRefused,
        false,
        "The tool declares a capability the guard does not know.",
        ASK,
    ),
    s(
        "workspace_not_granted",
        C::GuardRefused,
        false,
        "This workspace has not been granted to the session.",
        "Ask the user to grant the workspace, then call again.",
    ),
    s(
        "workspace_locked_by_subagent",
        C::GuardRefused,
        false,
        "A subagent holds this workspace.",
        "Wait for the subagent to finish (subagent_await), then call again.",
    ),
    s(
        "high_risk_command",
        C::GuardRefused,
        false,
        "This command needs a fresh approval.",
        "Ask the user to approve it, or use a safer command.",
    ),
    s(
        "lease_lost",
        C::GuardRefused,
        false,
        "This run no longer holds its execution lease.",
        "Stop; report to the user that the run was taken over.",
    ),
    s(
        "mcp_authority_revoked",
        C::GuardRefused,
        false,
        "Access to this MCP server was revoked.",
        "Do not call this server again; tell the user.",
    ),
    s(
        "tool_result_uncertain",
        C::Precondition,
        false,
        "The earlier run of this tool has no recorded result, so its effect is unknown.",
        "Ask the user whether to run it again; it needs a fresh approval.",
    ),
    s(
        "approval_mismatch",
        C::Precondition,
        false,
        "The approval no longer matches this execution context.",
        "Call the tool again so the user can approve it as it is now.",
    ),
    s(
        "execution_already_admitted",
        C::Precondition,
        false,
        "This authorization was already used.",
        "Check whether the earlier call took effect before calling again; ask the user if unsure.",
    ),
    s(
        "command_completion_failed",
        C::ToolError,
        false,
        "The command ran but its completion could not be recorded.",
        "Check the workspace state before repeating the command; tell the user if unsure.",
    ),
    s(
        "command_drain_timeout",
        C::Timeout,
        false,
        "The command's processes did not stop in time.",
        "Do not start the same command again; tell the user the workspace may still be busy.",
    ),
    s(
        "execution_context_mismatch",
        C::Precondition,
        false,
        "The recorded execution belongs to a different context.",
        "Call the tool again so it can be approved in the current context.",
    ),
    s(
        "invalid_execution",
        C::ToolError,
        false,
        "The execution record is not valid.",
        "Report this to the user; do not retry the call.",
    ),
    s(
        "receipt_conflict",
        C::Precondition,
        false,
        "Another execution already holds the receipt for this call.",
        "Wait and check the result of the earlier call before calling again.",
    ),
    s(
        "receipt_unavailable",
        C::ToolError,
        true,
        "The execution receipt could not be stored.",
        "Try the call once more; if it fails again, tell the user.",
    ),
    s(
        "recovery_history_unreadable",
        C::ToolError,
        false,
        "The history needed to recover this call could not be read.",
        "Tell the user; do not repeat the call until it is checked.",
    ),
    s(
        "service_closed",
        C::ToolError,
        false,
        "The service behind this tool has shut down.",
        "Stop using this tool and tell the user.",
    ),
    s(
        "write_tool_failed",
        C::ToolError,
        true,
        "The write could not be proposed.",
        "Read the error, adjust the proposal, and try once more.",
    ),
    s(
        "authorization_changed",
        C::Precondition,
        true,
        "The approval was for a different command.",
        "Call the command again so it can be approved as written.",
    ),
    s(
        "authorization_timeout",
        C::Timeout,
        true,
        "The approval was not given in time.",
        "Ask the user to approve, then call again.",
    ),
];

/// All rows, in table order.
pub(crate) fn all() -> impl Iterator<Item = &'static Spec> {
    [
        CORE,
        crate::failure_table_files::FILES,
        crate::failure_table_agents::AGENTS,
    ]
    .into_iter()
    .flatten()
}
