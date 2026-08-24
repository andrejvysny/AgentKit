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
} from "./tool.js";

export {
  AiProviderKindSchema,
  AiProviderConfigSchema,
  AiProviderCapabilitiesSchema,
  AiProviderModelSchema,
  AiChatRoleSchema,
  AiChatMessageSchema,
} from "./provider.js";

export { AiPromptPresetSchema, AiPromptContextBlockSchema } from "./prompt.js";

export {
  AiRunEventTypeSchema,
  AiRunEventBaseSchema,
  AiRunStartedEventSchema,
  AiRunMessageDeltaEventSchema,
  AiRunMessageCompletedEventSchema,
  AiRunToolRequestedEventSchema,
  AiRunToolRunningEventSchema,
  AiRunToolSucceededEventSchema,
  AiRunToolFailedEventSchema,
  AiRunWarningEventSchema,
  AiRunUsageEventSchema,
  AiRunCompletedEventSchema,
  AiRunFailedEventSchema,
  AiRunCancelledEventSchema,
  AiRunEventSchema,
} from "./run-events.js";

export { TaskEventEnvelopeSchema } from "./task-events.js";

// REST v1 DTOs. The route table (`REST_ROUTES`) and `REST_API_VERSION` are
// deliberately absent: this barrel is schemas and nothing else.
export {
  RunStatusDtoSchema,
  ProposalStatusDtoSchema,
  RiskLevelDtoSchema,
  ChatDtoSchema,
  MessageDtoSchema,
  MessagePageDtoSchema,
  RunDtoSchema,
  ProposalDecisionDtoSchema,
  ApplyOutcomeDtoSchema,
  ProposalDtoSchema,
  ToolEventDtoSchema,
  ToolDefinitionDtoSchema,
  CreateChatRequestSchema,
  SubmitMessageRequestSchema,
  SubmitMessageResponseSchema,
  ProposalDecisionRequestSchema,
  ApplyProposalRequestSchema,
  VersionDtoSchema,
  ProblemDetailsDtoSchema,
  RunEventFrameDtoSchema,
} from "./rest.js";
