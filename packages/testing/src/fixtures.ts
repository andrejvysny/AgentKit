import type {
  AiChatMessage,
  AiSourceRef,
  AiToolCall,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";

/** A `role: "user"` message with no frills. */
export function makeUserMessage(content: string): AiChatMessage {
  return { role: "user", content };
}

/**
 * A `role: "assistant"` message, optionally carrying `toolCalls` (as a real
 * turn does when the model asked for tools instead of, or alongside, text).
 */
export function makeAssistantMessage(
  content: string,
  toolCalls?: AiToolCall[],
): AiChatMessage {
  return {
    role: "assistant",
    content,
    ...(toolCalls === undefined ? {} : { toolCalls }),
  };
}

/** A single tool call, the shape a provider reports and a run-loop replays. */
export function makeToolCall(
  id: string,
  name: string,
  argumentsJson: string,
): AiToolCall {
  return { id, name, argumentsJson };
}

const DEFAULT_TOOL_LIMITS: AiToolLimits = {
  profile: "small",
  maxBytes: 16 * 1024,
  maxItems: 50,
};

/**
 * A valid {@link AiToolResult}, defaulted to a bare success so a test only has
 * to spell out the fields it cares about.
 */
export function makeToolResult(
  overrides: Partial<AiToolResult<unknown>> = {},
): AiToolResult<unknown> {
  return {
    ok: true,
    data: {},
    sources: [],
    warnings: [],
    truncated: false,
    limits: DEFAULT_TOOL_LIMITS,
    ...overrides,
  };
}

/** A citation, defaulted so a test only has to spell out what it's asserting on. */
export function makeSourceRef(
  overrides: Partial<AiSourceRef> = {},
): AiSourceRef {
  return {
    id: "src-1",
    kind: "test",
    label: "Test source",
    ...overrides,
  };
}
