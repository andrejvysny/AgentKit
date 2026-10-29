import type { AiRunToolRunningEvent } from "@agentkit/contracts";
import type { AiTool, AiToolRegistry } from "@agentkit/core";

const durableMutationWrappers = new WeakSet<AiTool>();

/** Internal identity proof; tool definitions and model metadata cannot set it. */
export function markDurableMutationWrapper(tool: AiTool): void {
  durableMutationWrappers.add(tool);
}

export function stampAdmissionProtocol(
  event: AiRunToolRunningEvent,
  registry: AiToolRegistry,
  attemptId: string,
): AiRunToolRunningEvent {
  const tool = registry.get(event.data.toolName);
  return {
    ...event,
    attemptId,
    data: {
      toolCallId: event.data.toolCallId,
      toolName: event.data.toolName,
      ...(tool !== undefined && durableMutationWrappers.has(tool)
        ? { admissionProtocol: "fenced-v1" as const }
        : {}),
    },
  };
}
