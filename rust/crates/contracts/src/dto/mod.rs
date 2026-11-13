//! Wire DTOs, one module per exported schema family. Hand-owned: edit here
//! alongside `schema_bundle.json` (ADR 0022).

mod agent_budgets_dto;
mod agent_definition_dto;
mod agent_dto;
mod agent_lifecycle_dto;
mod agent_revision_dto;
mod agent_revision_source_dto;
mod agent_skill_selection_dto;
mod agent_task_dto;
mod agent_task_projection_dto;
mod agent_task_status_dto;
mod ai_audio_part;
mod ai_chat_message;
mod ai_chat_role;
mod ai_citation;
mod ai_content_part;
mod ai_context_binding;
mod ai_context_binding_role;
mod ai_context_binding_status;
mod ai_context_size_preference;
mod ai_image_part;
mod ai_image_source;
mod ai_json_primitive_type;
mod ai_json_schema_object;
mod ai_message_content;
mod ai_prompt_context_block;
mod ai_prompt_preset;
mod ai_provider_capabilities;
mod ai_provider_config;
mod ai_provider_continuation;
mod ai_provider_kind;
mod ai_provider_model;
mod ai_run_cancelled_event;
mod ai_run_completed_event;
mod ai_run_context_admitted_event;
mod ai_run_context_compacted_event;
mod ai_run_event;
mod ai_run_event_base;
mod ai_run_event_type;
mod ai_run_failed_event;
mod ai_run_input_applied_event;
mod ai_run_message_completed_event;
mod ai_run_message_delta_event;
mod ai_run_message_reasoning_delta_event;
mod ai_run_progress_event;
mod ai_run_started_event;
mod ai_run_tool_failed_event;
mod ai_run_tool_preparing_event;
mod ai_run_tool_requested_event;
mod ai_run_tool_running_event;
mod ai_run_tool_succeeded_event;
mod ai_run_usage_event;
mod ai_run_verification_event;
mod ai_run_waiting_event;
mod ai_run_warning_event;
mod ai_source_ref;
mod ai_text_part;
mod ai_tool_call;
mod ai_tool_definition;
mod ai_tool_effect;
mod ai_tool_envelope;
mod ai_tool_error_data;
mod ai_tool_error_phase;
mod ai_tool_limits;
mod ai_tool_result;
mod ai_tool_status;
mod apply_outcome_dto;
mod apply_proposal_request;
mod approval_grant_ref_dto;
mod chat_dto;
mod connection_record;
mod connection_status;
mod context_unit_dto;
mod create_agent_request;
mod create_chat_request;
mod create_mcp_server_request;
mod create_provider_request;
mod fork_chat_request;
mod grant_allowance_request;
mod managed_connection_config;
mod mcp_http_transport_dto;
mod mcp_server_dto;
mod mcp_stdio_transport_dto;
mod mcp_transport_dto;
mod message_dto;
mod message_page_dto;
mod message_search_hit_dto;
mod message_search_response;
mod model_dto;
mod problem_details_dto;
mod prompt_template;
mod proposal_decision_dto;
mod proposal_decision_request;
mod proposal_dto;
mod proposal_status_dto;
mod provider_dto;
mod regenerate_message_request;
mod risk_level_dto;
mod run_content_selection;
mod run_dto;
mod run_event_frame_dto;
mod run_profile_explanation_dto;
mod run_status_dto;
mod selected_source_dto;
mod set_agent_model_request;
mod settings_dto;
mod submit_message_request;
mod submit_message_response;
mod submit_teammate_task_request;
mod submit_teammate_task_response;
mod task_event_approval;
mod task_event_envelope;
mod task_event_page_dto;
mod task_event_status;
mod task_event_usage;
mod task_event_usage_totals_dto;
mod task_profile_overrides_dto;
mod task_projection_approval_dto;
mod test_provider_response;
mod tool_calling_mode_dto;
mod tool_definition_dto;
mod tool_event_dto;
mod update_agent_request;
mod update_chat_request;
mod update_mcp_server_request;
mod update_provider_request;
mod update_settings_request;
mod version_dto;
mod write_allowance_dto;
mod write_allowance_list_response;
mod write_policy_mode_dto;

pub use agent_budgets_dto::AgentBudgetsDto;
pub use agent_definition_dto::AgentDefinitionDto;
pub use agent_dto::AgentDto;
pub use agent_lifecycle_dto::AgentLifecycleDto;
pub use agent_revision_dto::AgentRevisionDto;
pub use agent_revision_source_dto::AgentRevisionSourceDto;
pub use agent_skill_selection_dto::AgentSkillSelectionDto;
pub use agent_task_dto::AgentTaskDto;
pub use agent_task_projection_dto::AgentTaskProjectionDto;
pub use agent_task_status_dto::AgentTaskStatusDto;
pub use ai_audio_part::{AiAudioPart, AiAudioPartFormat, AiAudioPartType};
pub use ai_chat_message::AiChatMessage;
pub use ai_chat_role::AiChatRole;
pub use ai_citation::AiCitation;
pub use ai_content_part::AiContentPart;
pub use ai_context_binding::AiContextBinding;
pub use ai_context_binding_role::AiContextBindingRole;
pub use ai_context_binding_status::AiContextBindingStatus;
pub use ai_context_size_preference::AiContextSizePreference;
pub use ai_image_part::{AiImagePart, AiImagePartDetail, AiImagePartType};
pub use ai_image_source::{
    AiImageSource, AiImageSourceData, AiImageSourceDataKind, AiImageSourceRef,
    AiImageSourceRefKind, AiImageSourceUrl, AiImageSourceUrlKind,
};
pub use ai_json_primitive_type::AiJsonPrimitiveType;
pub use ai_json_schema_object::AiJsonSchemaObject;
pub use ai_message_content::AiMessageContent;
pub use ai_prompt_context_block::AiPromptContextBlock;
pub use ai_prompt_preset::AiPromptPreset;
pub use ai_provider_capabilities::AiProviderCapabilities;
pub use ai_provider_config::AiProviderConfig;
pub use ai_provider_continuation::{
    AiProviderContinuation, AiProviderContinuationRunContent, AiProviderContinuationScope,
    AiProviderContinuationScopeProtocol, AiProviderContinuationVersion,
};
pub use ai_provider_kind::AiProviderKind;
pub use ai_provider_model::AiProviderModel;
pub use ai_run_cancelled_event::{AiRunCancelledEvent, AiRunCancelledEventType};
pub use ai_run_completed_event::{
    AiRunCompletedEvent, AiRunCompletedEventData, AiRunCompletedEventType,
};
pub use ai_run_context_admitted_event::{
    AiRunContextAdmittedEvent, AiRunContextAdmittedEventData,
    AiRunContextAdmittedEventDataWindowSource, AiRunContextAdmittedEventType,
};
pub use ai_run_context_compacted_event::{
    AiRunContextCompactedEvent, AiRunContextCompactedEventData,
    AiRunContextCompactedEventDataReason, AiRunContextCompactedEventType,
};
pub use ai_run_event::AiRunEvent;
pub use ai_run_event_base::AiRunEventBase;
pub use ai_run_event_type::AiRunEventType;
pub use ai_run_failed_event::{AiRunFailedEvent, AiRunFailedEventData, AiRunFailedEventType};
pub use ai_run_input_applied_event::{
    AiRunInputAppliedEvent, AiRunInputAppliedEventData, AiRunInputAppliedEventType,
};
pub use ai_run_message_completed_event::{
    AiRunMessageCompletedEvent, AiRunMessageCompletedEventData, AiRunMessageCompletedEventType,
};
pub use ai_run_message_delta_event::{
    AiRunMessageDeltaEvent, AiRunMessageDeltaEventData, AiRunMessageDeltaEventType,
};
pub use ai_run_message_reasoning_delta_event::{
    AiRunMessageReasoningDeltaEvent, AiRunMessageReasoningDeltaEventData,
    AiRunMessageReasoningDeltaEventType,
};
pub use ai_run_progress_event::{
    AiRunProgressEvent, AiRunProgressEventData, AiRunProgressEventDataPhase, AiRunProgressEventType,
};
pub use ai_run_started_event::{AiRunStartedEvent, AiRunStartedEventData, AiRunStartedEventType};
pub use ai_run_tool_failed_event::{
    AiRunToolFailedEvent, AiRunToolFailedEventData, AiRunToolFailedEventDataStatus,
    AiRunToolFailedEventType,
};
pub use ai_run_tool_preparing_event::{
    AiRunToolPreparingEvent, AiRunToolPreparingEventData, AiRunToolPreparingEventType,
};
pub use ai_run_tool_requested_event::{
    AiRunToolRequestedEvent, AiRunToolRequestedEventData, AiRunToolRequestedEventType,
};
pub use ai_run_tool_running_event::{
    AiRunToolRunningEvent, AiRunToolRunningEventData, AiRunToolRunningEventDataAdmissionProtocol,
    AiRunToolRunningEventType,
};
pub use ai_run_tool_succeeded_event::{
    AiRunToolSucceededEvent, AiRunToolSucceededEventData, AiRunToolSucceededEventDataStatus,
    AiRunToolSucceededEventType,
};
pub use ai_run_usage_event::{
    AiRunUsageEvent, AiRunUsageEventData, AiRunUsageEventDataSource, AiRunUsageEventType,
};
pub use ai_run_verification_event::{
    AiRunVerificationEvent, AiRunVerificationEventData, AiRunVerificationEventDataStatus,
    AiRunVerificationEventType,
};
pub use ai_run_waiting_event::{
    AiRunWaitingEvent, AiRunWaitingEventData, AiRunWaitingEventDataKind,
    AiRunWaitingEventDataStanding, AiRunWaitingEventType,
};
pub use ai_run_warning_event::{AiRunWarningEvent, AiRunWarningEventData, AiRunWarningEventType};
pub use ai_source_ref::AiSourceRef;
pub use ai_text_part::{AiTextPart, AiTextPartType};
pub use ai_tool_call::AiToolCall;
pub use ai_tool_definition::AiToolDefinition;
pub use ai_tool_effect::AiToolEffect;
pub use ai_tool_envelope::{AiToolEnvelope, AiToolEnvelopeStatus};
pub use ai_tool_error_data::AiToolErrorData;
pub use ai_tool_error_phase::AiToolErrorPhase;
pub use ai_tool_limits::AiToolLimits;
pub use ai_tool_result::{AiToolResult, AiToolResultStatus};
pub use ai_tool_status::AiToolStatus;
pub use apply_outcome_dto::{ApplyOutcomeDto, ApplyOutcomeDtoFailedOpsItem, ApplyOutcomeDtoStatus};
pub use apply_proposal_request::ApplyProposalRequest;
pub use approval_grant_ref_dto::ApprovalGrantRefDto;
pub use chat_dto::ChatDto;
pub use connection_record::ConnectionRecord;
pub use connection_status::ConnectionStatus;
pub use context_unit_dto::{ContextUnitDto, ContextUnitDtoKind};
pub use create_agent_request::CreateAgentRequest;
pub use create_chat_request::CreateChatRequest;
pub use create_mcp_server_request::CreateMcpServerRequest;
pub use create_provider_request::CreateProviderRequest;
pub use fork_chat_request::ForkChatRequest;
pub use grant_allowance_request::GrantAllowanceRequest;
pub use managed_connection_config::{
    ManagedConnectionConfig, ManagedConnectionConfigDiscovery, ManagedConnectionConfigResilience,
};
pub use mcp_http_transport_dto::{McpHttpTransportDto, McpHttpTransportDtoKind};
pub use mcp_server_dto::McpServerDto;
pub use mcp_stdio_transport_dto::{McpStdioTransportDto, McpStdioTransportDtoKind};
pub use mcp_transport_dto::McpTransportDto;
pub use message_dto::{MessageDto, MessageDtoRole};
pub use message_page_dto::MessagePageDto;
pub use message_search_hit_dto::MessageSearchHitDto;
pub use message_search_response::MessageSearchResponse;
pub use model_dto::ModelDto;
pub use problem_details_dto::ProblemDetailsDto;
pub use prompt_template::{
    PromptTemplate, PromptTemplateMessagesItem, PromptTemplateMessagesItemRole,
    PromptTemplateOrigin, PromptTemplateOriginKind,
};
pub use proposal_decision_dto::{ProposalDecisionDto, ProposalDecisionDtoActor};
pub use proposal_decision_request::ProposalDecisionRequest;
pub use proposal_dto::ProposalDto;
pub use proposal_status_dto::ProposalStatusDto;
pub use provider_dto::ProviderDto;
pub use regenerate_message_request::{
    RegenerateMessageRequest, RegenerateMessageRequestReasoningEffort,
};
pub use risk_level_dto::RiskLevelDto;
pub use run_content_selection::{
    RunContentSelection, RunContentSelectionSkillsItem, RunContentSelectionTemplate,
};
pub use run_dto::RunDto;
pub use run_event_frame_dto::RunEventFrameDto;
pub use run_profile_explanation_dto::RunProfileExplanationDto;
pub use run_status_dto::RunStatusDto;
pub use selected_source_dto::{SelectedSourceDto, SelectedSourceDtoKind};
pub use set_agent_model_request::SetAgentModelRequest;
pub use settings_dto::SettingsDto;
pub use submit_message_request::{SubmitMessageRequest, SubmitMessageRequestReasoningEffort};
pub use submit_message_response::SubmitMessageResponse;
pub use submit_teammate_task_request::SubmitTeammateTaskRequest;
pub use submit_teammate_task_response::SubmitTeammateTaskResponse;
pub use task_event_approval::{TaskEventApproval, TaskEventApprovalState};
pub use task_event_envelope::TaskEventEnvelope;
pub use task_event_page_dto::TaskEventPageDto;
pub use task_event_status::TaskEventStatus;
pub use task_event_usage::TaskEventUsage;
pub use task_event_usage_totals_dto::TaskEventUsageTotalsDto;
pub use task_profile_overrides_dto::{
    TaskProfileOverridesDto, TaskProfileOverridesDtoTruncation,
    TaskProfileOverridesDtoTruncationMode,
};
pub use task_projection_approval_dto::{TaskProjectionApprovalDto, TaskProjectionApprovalDtoState};
pub use test_provider_response::TestProviderResponse;
pub use tool_calling_mode_dto::ToolCallingModeDto;
pub use tool_definition_dto::ToolDefinitionDto;
pub use tool_event_dto::ToolEventDto;
pub use update_agent_request::UpdateAgentRequest;
pub use update_chat_request::UpdateChatRequest;
pub use update_mcp_server_request::UpdateMcpServerRequest;
pub use update_provider_request::UpdateProviderRequest;
pub use update_settings_request::UpdateSettingsRequest;
pub use version_dto::VersionDto;
pub use write_allowance_dto::WriteAllowanceDto;
pub use write_allowance_list_response::WriteAllowanceListResponse;
pub use write_policy_mode_dto::WritePolicyModeDto;
