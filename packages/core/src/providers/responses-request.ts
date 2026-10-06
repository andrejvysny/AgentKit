import Ajv from "ajv";
import {
  AiProviderContinuationSchema,
  IMAGE_URL_PATTERN,
  MEDIA_TYPE_PATTERN,
  type AiChatMessage,
  type AiProviderContinuation,
  type AiToolCall,
} from "@agentkit/contracts";
import type { AiChatRequest } from "./client.js";
import { RESPONSES_TOOL_NAMESPACE, responsesTools } from "./responses-tools.js";
import {
  boundedJson,
  MAX_RESPONSES_ITEMS,
  object,
  responsesError,
  type AiProviderConnectionIdentity,
} from "./responses-types.js";

const validateContinuation = new Ajv({
  strict: false,
}).compile<AiProviderContinuation>(AiProviderContinuationSchema);

export async function buildResponsesRequest(
  input: AiChatRequest,
  identity: AiProviderConnectionIdentity,
  account: boolean,
): Promise<Record<string, unknown>> {
  const tools = responsesTools(input.tools);
  if (
    account &&
    (input.temperature !== undefined || input.maxOutputTokens !== undefined)
  ) {
    responsesError("unsupported_parameter");
  }
  let items: Record<string, unknown>[];
  if (input.continuation) {
    await checkContinuation(input, identity);
    items = [
      ...structuredClone(input.continuation.inputItems),
      ...mapMessages(input.messages.slice(input.continuation.messageCount)),
    ];
  } else {
    if (input.continuationRequired) responsesError("continuation_required");
    items = mapMessages(input.messages);
  }
  if (items.length > MAX_RESPONSES_ITEMS)
    responsesError("state_limit_exceeded");
  const instructions = input.messages
    .filter((message) => message.role === "system")
    .map(textContent)
    .join("\n\n");
  const body: Record<string, unknown> = {
    model: input.model,
    input: items,
    stream: true,
    store: false,
    include: ["reasoning.encrypted_content"],
    ...(instructions ? { instructions } : {}),
    ...(tools.length ? { tools } : {}),
  };
  if (!account) {
    if (input.temperature !== undefined) body.temperature = input.temperature;
    if (input.maxOutputTokens !== undefined)
      body.max_output_tokens = input.maxOutputTokens;
  }
  boundedJson(body);
  return body;
}

async function checkContinuation(
  input: AiChatRequest,
  identity: AiProviderConnectionIdentity,
): Promise<void> {
  const state = input.continuation;
  boundedJson(state);
  if (!validateContinuation(state) || !state || !input.continuationScope)
    responsesError("invalid_continuation");
  const expected = {
    ...identity,
    ...input.continuationScope,
    model: input.model,
  };
  for (const key of Object.keys(expected) as Array<keyof typeof expected>) {
    if (state.scope[key] !== expected[key])
      responsesError("continuation_scope_mismatch");
  }
  if (
    state.messageCount > input.messages.length ||
    state.messageDigest !==
      (await messageDigest(input.messages.slice(0, state.messageCount)))
  ) {
    responsesError("continuation_history_mismatch");
  }
  for (const item of state.inputItems) validateReplayItem(object(item));
}

export async function makeResponsesContinuation(
  input: AiChatRequest,
  identity: AiProviderConnectionIdentity,
  body: Record<string, unknown>,
  output: Record<string, unknown>[],
  content: string,
  toolCalls: AiToolCall[],
): Promise<AiProviderContinuation | undefined> {
  if (!input.continuationScope) {
    if (output.some((item) => item.type === "reasoning"))
      responsesError("continuation_scope_required");
    return undefined;
  }
  const assistant: AiChatMessage = {
    role: "assistant",
    content,
    ...(toolCalls.length ? { toolCalls } : {}),
  };
  const state: AiProviderContinuation = {
    version: 1,
    scope: { ...identity, ...input.continuationScope, model: input.model },
    messageCount: input.messages.length + 1,
    messageDigest: await messageDigest([...input.messages, assistant]),
    inputItems: [
      ...(body.input as Record<string, unknown>[]),
      ...structuredClone(output),
    ],
  };
  boundedJson(state);
  if (!validateContinuation(state)) responsesError("state_limit_exceeded");
  return state;
}

async function messageDigest(messages: AiChatMessage[]): Promise<string> {
  const stable = messages.map((message) => ({
    role: message.role,
    content: message.content,
    ...(message.toolCallId ? { toolCallId: message.toolCallId } : {}),
    ...(message.toolCalls?.length
      ? {
          toolCalls: message.toolCalls.map((call) => ({
            id: call.id,
            name: call.name,
            argumentsJson: call.argumentsJson,
          })),
        }
      : {}),
  }));
  const digest = await globalThis.crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(boundedJson(stable)),
  );
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function mapMessages(messages: AiChatMessage[]): Record<string, unknown>[] {
  const items: Record<string, unknown>[] = [];
  for (const message of messages) {
    if (message.role === "system") {
      textContent(message);
      continue;
    }
    if (message.role === "tool") {
      if (!message.toolCallId) responsesError("missing_tool_call_id");
      items.push({
        type: "function_call_output",
        call_id: message.toolCallId,
        output: textContent(message),
      });
      continue;
    }
    const content = contentParts(message);
    if (content.length)
      items.push({ type: "message", role: message.role, content });
    for (const call of message.toolCalls ?? []) {
      if (message.role !== "assistant" || !call.id || !call.name)
        responsesError("invalid_tool_history");
      items.push({
        type: "function_call",
        call_id: call.id,
        namespace: RESPONSES_TOOL_NAMESPACE,
        name: call.name,
        arguments: call.argumentsJson,
      });
    }
  }
  return items;
}

function textContent(message: AiChatMessage): string {
  if (typeof message.content === "string") return message.content;
  return message.content
    .map((part) => {
      if (part.type !== "text") responsesError("unsupported_modality");
      return part.text;
    })
    .join("");
}

function contentParts(message: AiChatMessage): Record<string, unknown>[] {
  const parts =
    typeof message.content === "string"
      ? [{ type: "text" as const, text: message.content }]
      : message.content;
  return parts.map((part) => {
    if (part.type === "text") return { type: "input_text", text: part.text };
    if (part.type !== "image" || message.role !== "user")
      responsesError("unsupported_modality");
    const source = part.source;
    let url: string;
    if (source.kind === "url" && new RegExp(IMAGE_URL_PATTERN).test(source.url))
      url = source.url;
    else if (
      source.kind === "data" &&
      new RegExp(MEDIA_TYPE_PATTERN).test(source.mediaType)
    ) {
      url = `data:${source.mediaType};base64,${source.base64}`;
    } else responsesError("unsupported_modality");
    return {
      type: "input_image",
      image_url: url,
      detail: part.detail ?? "auto",
    };
  });
}

function validateReplayItem(item: Record<string, unknown>): void {
  if (
    !["message", "function_call", "function_call_output", "reasoning"].includes(
      String(item.type),
    )
  ) {
    responsesError("unsupported_continuation_item");
  }
  if (
    item.type === "reasoning" &&
    (typeof item.encrypted_content !== "string" || !item.encrypted_content)
  ) {
    responsesError("continuation_required");
  }
}
