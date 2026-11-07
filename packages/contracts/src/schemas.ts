/**
 * Value barrel: every TypeBox schema in this package, and nothing else.
 *
 * The types in `index.ts` are all derived from these schemas (`Static<typeof X>`),
 * so this module is the machine-readable half of the same contract — import it to
 * enumerate the JSON Schemas (validation, codegen, docs) without pulling in the
 * type-only surface. Every entry here is a runtime value.
 */

export {
  AiJsonPrimitiveTypeSchema,
  AiJsonSchemaObjectSchema,
} from "./json-schema.js";

export { AiSourceRefSchema } from "./source-ref.js";

export {
  AiContextBindingRoleSchema,
  AiContextBindingStatusSchema,
  AiContextBindingSchema,
} from "./context-binding.js";

export {
  AiTextPartSchema,
  AiImageSourceSchema,
  AiImagePartSchema,
  AiAudioPartSchema,
  AiContentPartSchema,
  AiMessageContentSchema,
} from "./content.js";

export {
  AiToolEffectSchema,
  AiToolStatusSchema,
  AiToolDefinitionSchema,
  AiToolCallSchema,
  AiContextSizePreferenceSchema,
  AiToolLimitsSchema,
  AiToolResultSchema,
  AiToolEnvelopeSchema,
  AiToolErrorPhaseSchema,
  AiToolErrorDataSchema,
} from "./tool.js";

export {
  AiProviderKindSchema,
  AiProviderConfigSchema,
  AiProviderCapabilitiesSchema,
  AiProviderModelSchema,
  AiChatRoleSchema,
  AiChatMessageSchema,
} from "./provider.js";

export {
  AiPromptPresetSchema,
  AiPromptContextBlockSchema,
  RunContentSelectionSchema,
} from "./prompt.js";

export {
  AiRunEventTypeSchema,
  AiRunEventBaseSchema,
  AiRunStartedEventSchema,
  AiCitationSchema,
  AiRunMessageDeltaEventSchema,
  AiRunMessageReasoningDeltaEventSchema,
  AiRunMessageCompletedEventSchema,
  AiRunToolPreparingEventSchema,
  AiRunToolRequestedEventSchema,
  AiRunToolRunningEventSchema,
  AiRunToolSucceededEventSchema,
  AiRunToolFailedEventSchema,
  AiRunWarningEventSchema,
  AiRunUsageEventSchema,
  AiRunContextCompactedEventSchema,
  AiRunContextAdmittedEventSchema,
  AiRunVerificationEventSchema,
  AiRunProgressEventSchema,
  AiRunInputAppliedEventSchema,
  AiRunWaitingEventSchema,
  AiRunCompletedEventSchema,
  AiRunFailedEventSchema,
  AiRunCancelledEventSchema,
  AiRunEventSchema,
} from "./run-events.js";

export {
  TaskEventEnvelopeSchema,
  TaskEventStatusSchema,
  TaskEventApprovalSchema,
  TaskEventUsageSchema,
} from "./task-events.js";

// AGENTKIT-90: the persistent-teammate surface (agent lifecycle +
// definition versions, task submission/status/history, event pages,
// effective-run-profile inspection).
export {
  AgentBudgetsDtoSchema,
  AgentSkillSelectionDtoSchema,
  AgentDefinitionDtoSchema,
  AgentLifecycleDtoSchema,
  AgentDtoSchema,
  AgentRevisionSourceDtoSchema,
  AgentRevisionDtoSchema,
  CreateAgentRequestSchema,
  UpdateAgentRequestSchema,
  SetAgentModelRequestSchema,
  TaskProfileOverridesDtoSchema,
  ContextUnitDtoSchema,
  SelectedSourceDtoSchema,
  ApprovalGrantRefDtoSchema,
  SubmitTeammateTaskRequestSchema,
  AgentTaskStatusDtoSchema,
  AgentTaskDtoSchema,
  SubmitTeammateTaskResponseSchema,
  TaskEventPageDtoSchema,
  TaskEventUsageTotalsDtoSchema,
  TaskProjectionApprovalDtoSchema,
  AgentTaskProjectionDtoSchema,
  RunProfileExplanationDtoSchema,
} from "./teammates.js";

// REST v1 DTOs. The route table (`REST_ROUTES`) and `REST_API_VERSION` are
// deliberately absent: this barrel is schemas and nothing else.
export {
  RunStatusDtoSchema,
  ProposalStatusDtoSchema,
  RiskLevelDtoSchema,
  WritePolicyModeDtoSchema,
  ToolCallingModeDtoSchema,
  ChatDtoSchema,
  MessageDtoSchema,
  MessagePageDtoSchema,
  MessageSearchHitDtoSchema,
  MessageSearchResponseSchema,
  RunDtoSchema,
  ProposalDecisionDtoSchema,
  ApplyOutcomeDtoSchema,
  ProposalDtoSchema,
  ToolEventDtoSchema,
  ToolDefinitionDtoSchema,
  ProviderDtoSchema,
  ModelDtoSchema,
  SettingsDtoSchema,
  WriteAllowanceDtoSchema,
  WriteAllowanceListResponseSchema,
  McpStdioTransportDtoSchema,
  McpHttpTransportDtoSchema,
  McpTransportDtoSchema,
  McpServerDtoSchema,
  CreateChatRequestSchema,
  UpdateChatRequestSchema,
  SubmitMessageRequestSchema,
  SubmitMessageResponseSchema,
  RegenerateMessageRequestSchema,
  ForkChatRequestSchema,
  ProposalDecisionRequestSchema,
  ApplyProposalRequestSchema,
  CreateProviderRequestSchema,
  UpdateProviderRequestSchema,
  TestProviderResponseSchema,
  UpdateSettingsRequestSchema,
  GrantAllowanceRequestSchema,
  CreateMcpServerRequestSchema,
  UpdateMcpServerRequestSchema,
  VersionDtoSchema,
  ProblemDetailsDtoSchema,
  RunEventFrameDtoSchema,
} from "./rest.js";

export { AiProviderContinuationSchema } from "./provider-continuation.js";

export {
  ManagedConnectionConfigSchema,
  ConnectionStatusSchema,
  ConnectionRecordSchema,
  PromptTemplateSchema,
} from "./managed-content.js";
