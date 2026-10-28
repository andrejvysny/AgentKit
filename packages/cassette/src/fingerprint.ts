import { createHash } from "node:crypto";
import type { AiChatMessage } from "@agentkit/contracts";
import type { AiChatRequest, AiToolDefinition } from "@agentkit/core";
import type { RecordedMessageShape, RecordedProviderRequest } from "./types.js";

/** Canonical JSON: object keys sorted, so equal values hash equal. */
export function canonicalJson(value: unknown): string {
  return JSON.stringify(sortValue(value));
}

function sortValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortValue);
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const key of Object.keys(value as Record<string, unknown>).sort())
      out[key] = sortValue((value as Record<string, unknown>)[key]);
    return out;
  }
  return value;
}

export function sha256(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

function sha256Json(value: unknown): string {
  return sha256(canonicalJson(value));
}

/**
 * Hash of the tool definitions offered to the model — part of what makes a
 * provider call the *same* call. Names, descriptions and schemas all change
 * model behavior, so all three enter the hash.
 */
export function fingerprintToolDefinitions(
  tools: AiToolDefinition[] | undefined,
): string {
  if (!tools || tools.length === 0) return sha256("[]");
  return sha256Json(
    tools.map((t) => ({
      name: t.name,
      version: t.version,
      description: t.description,
      capability: t.capability,
      effect: t.effect,
      inputSchema: t.inputSchema,
      outputSchema: t.outputSchema,
      timeoutMs: t.timeoutMs,
    })),
  );
}

/** Hash of one call's identity: tool name + argument values. */
export function fingerprintToolCall(
  name: string,
  args: unknown,
): { name: string; argsSha256: string } {
  return { name, argsSha256: sha256Json(args ?? null) };
}

/**
 * The matchable view of a conversation.
 *
 * Under `metadata` capture, assistant/tool message *content* is structure
 * only: on replay the assistant text comes from the recorded stream and the
 * tool text from the recorded result, so hashing it would compare recorded
 * text against a placeholder. User/system content is test-supplied and
 * identical on both sides, so its hash is a genuine, safe (hash-only) check.
 *
 * Under `content` capture every role's content is hashed — the recorded text
 * and the replayed text are both real, so the comparison is exact.
 */
export function messageShape(
  message: AiChatMessage,
  capture: "metadata" | "content",
): RecordedMessageShape {
  const shape: RecordedMessageShape = { role: message.role };
  const structureOnly =
    capture === "metadata" &&
    (message.role === "assistant" || message.role === "tool");
  if (
    !structureOnly &&
    message.content !== undefined &&
    message.content !== null
  ) {
    shape.contentSha256 = sha256(String(message.content));
    if (capture === "content") shape.contentText = String(message.content);
  }
  if (message.toolCalls && message.toolCalls.length > 0)
    shape.toolCalls = message.toolCalls.map((call) => ({
      id: call.id,
      name: call.name,
      argsSha256: sha256(call.argumentsJson),
    }));
  if (message.toolCallId !== undefined) shape.toolCallId = message.toolCallId;
  if (message.name !== undefined) shape.name = message.name;
  return shape;
}

/**
 * The request view stored on a provider effect. Deliberately contains only
 * non-secret fields: no message bodies, no continuation state (presence is
 * noted, contents never), no transport concerns — headers and credentials are
 * not part of `AiChatRequest` and are never consulted.
 */
export function providerRequestView(
  providerId: string,
  providerKind: string,
  request: AiChatRequest,
  capture: "metadata" | "content",
): RecordedProviderRequest {
  const continuation = request.continuation;
  const scope = request.continuationScope;
  return {
    providerId,
    providerKind,
    model: request.model,
    ...(request.temperature !== undefined
      ? { temperature: request.temperature }
      : {}),
    ...(request.maxOutputTokens !== undefined
      ? { maxOutputTokens: request.maxOutputTokens }
      : {}),
    ...(request.reasoningEffort !== undefined
      ? { reasoningEffort: request.reasoningEffort }
      : {}),
    toolsFingerprint: fingerprintToolDefinitions(request.tools),
    messageShape: request.messages.map((m) => messageShape(m, capture)),
    continuation: {
      present: (continuation?.inputItems?.length ?? 0) > 0,
      ...(scope?.chatId !== undefined ? { scopeChatId: scope.chatId } : {}),
      ...(scope?.branchId !== undefined
        ? { scopeBranchId: scope.branchId }
        : {}),
    },
    hasRunContent: request.runContent !== undefined,
  };
}

/** Structural fingerprint: what `metadata` capture matches on. */
export function structuralFingerprint(view: RecordedProviderRequest): string {
  return sha256Json({
    providerId: view.providerId,
    providerKind: view.providerKind,
    model: view.model,
    temperature: view.temperature,
    maxOutputTokens: view.maxOutputTokens,
    reasoningEffort: view.reasoningEffort,
    toolsFingerprint: view.toolsFingerprint,
    continuation: { present: view.continuation.present },
    shape: view.messageShape.map((m) => ({
      role: m.role,
      contentSha256: m.contentSha256,
      toolCalls: m.toolCalls,
      toolCallId: m.toolCallId,
      name: m.name,
    })),
  });
}

/** Full fingerprint: adds continuation scope identity; used with `content`. */
export function fullFingerprint(view: RecordedProviderRequest): string {
  return sha256Json({ structural: structuralFingerprint(view), view });
}
