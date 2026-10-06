import {
  isAiProviderContinuation,
  isAiProviderContinuationScope,
  type AiProviderContinuation,
} from "@agentkit/contracts";
import { AgentKitHostError } from "../errors.js";
import type { ProviderContinuationScope } from "../ports/provider-continuation-store.js";

export const MAX_PROVIDER_CONTINUATION_BYTES = 1024 * 1024;
export const MAX_PROVIDER_CONTINUATION_ITEMS = 1024;

export function providerContinuationScopesEqual(
  left: ProviderContinuationScope,
  right: ProviderContinuationScope,
): boolean {
  return (
    left.providerId === right.providerId &&
    left.protocol === right.protocol &&
    left.connectionId === right.connectionId &&
    left.generation === right.generation &&
    left.chatId === right.chatId &&
    left.branchId === right.branchId &&
    left.model === right.model
  );
}

export function validateProviderContinuationScope(
  value: unknown,
): ProviderContinuationScope {
  try {
    if (
      !isAiProviderContinuationScope(value) ||
      !Number.isSafeInteger(value.generation)
    )
      invalid();
    return JSON.parse(encode(value)) as ProviderContinuationScope;
  } catch {
    return invalid();
  }
}

export function encodeProviderContinuation(value: unknown): string {
  try {
    if (
      !isAiProviderContinuation(value) ||
      !Number.isSafeInteger(value.messageCount) ||
      !Number.isSafeInteger(value.scope.generation) ||
      value.inputItems.length > MAX_PROVIDER_CONTINUATION_ITEMS
    )
      invalid();
    return encode(value);
  } catch {
    return invalid();
  }
}

export function decodeProviderContinuation(
  json: string,
): AiProviderContinuation {
  try {
    assertEncodedSize(json);
    const value: unknown = JSON.parse(json);
    encodeProviderContinuation(value);
    return value as AiProviderContinuation;
  } catch {
    return invalid();
  }
}

export function decodeProviderContinuationScope(
  json: string,
): ProviderContinuationScope {
  try {
    assertEncodedSize(json);
    return validateProviderContinuationScope(JSON.parse(json));
  } catch {
    return invalid();
  }
}

export function validateProviderContinuationAnchors(
  chatId: unknown,
  anchors: unknown,
): string[] {
  if (
    typeof chatId !== "string" ||
    chatId.length === 0 ||
    !Array.isArray(anchors) ||
    anchors.length > MAX_PROVIDER_CONTINUATION_ITEMS ||
    anchors.some((id: unknown) => typeof id !== "string" || id.length === 0)
  )
    invalid();
  return [...new Set(anchors as string[])];
}

export function providerContinuationScopeMismatch(): never {
  throw new AgentKitHostError(
    "provider_continuation_scope_mismatch",
    "Provider continuation does not match its bound run scope.",
  );
}

export function providerContinuationStateMissing(): never {
  throw new AgentKitHostError(
    "provider_continuation_state_missing",
    "Required private provider continuation state is missing.",
  );
}

function encode(value: unknown): string {
  const json = JSON.stringify(value, (_key: string, entry: unknown) => {
    if (
      entry === undefined ||
      typeof entry === "function" ||
      typeof entry === "symbol" ||
      typeof entry === "bigint" ||
      (typeof entry === "number" && !Number.isFinite(entry))
    )
      invalid();
    return entry;
  });
  assertEncodedSize(json);
  return json;
}

function assertEncodedSize(json: string): void {
  if (
    typeof json !== "string" ||
    json.length > MAX_PROVIDER_CONTINUATION_BYTES ||
    new TextEncoder().encode(json).byteLength > MAX_PROVIDER_CONTINUATION_BYTES
  )
    invalid();
}

function invalid(): never {
  throw new AgentKitHostError(
    "invalid_provider_continuation",
    "Provider continuation must match the private schema and stay within 1 MiB and 1024 items.",
  );
}
