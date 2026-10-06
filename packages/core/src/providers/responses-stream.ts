import type { AiToolCall, AiToolDefinition } from "@agentkit/contracts";
import { responseToolCall } from "./responses-tools.js";
import {
  boundedJson,
  MAX_RESPONSES_ITEMS,
  object,
  responsesError,
} from "./responses-types.js";

interface ItemState {
  id: string;
  index: number;
  item: Record<string, unknown>;
  arguments: string;
  texts: Map<number, string>;
  done: boolean;
}

export interface ResponsesResult {
  output: Record<string, unknown>[];
  content: string;
  tools: AiToolCall[];
  usage?: Record<string, unknown>;
}

/** Assemblers are keyed by both index and item id; neither may be rebound. */
export class ResponsesStreamState {
  private readonly byIndex = new Map<number, ItemState>();
  private readonly byId = new Map<string, ItemState>();
  private terminal?: Record<string, unknown>;
  private emitted = "";
  private bytes = 0;
  private observedUsage?: Record<string, unknown>;

  usage(): Record<string, unknown> | undefined {
    return this.observedUsage;
  }

  accept(value: unknown): string | undefined {
    const event = object(value);
    this.bytes += new TextEncoder().encode(boundedJson(event)).length;
    if (this.bytes > 16 * 1024 * 1024) responsesError("stream_limit_exceeded");
    if (typeof event.type !== "string") responsesError("malformed_response");
    this.captureUsage(event);
    if (event.type === "response.failed" || event.type === "error") {
      const error =
        event.type === "error"
          ? event
          : object(object(event.response).error ?? {});
      responsesError(classifyResponseError(error.code));
    }
    if (event.type === "response.incomplete")
      responsesError("response_incomplete");
    if (this.terminal) responsesError("event_after_terminal");
    if (event.type === "response.completed") {
      this.terminal = object(event.response);
      if (
        this.terminal.status !== "completed" ||
        this.terminal.error ||
        this.terminal.incomplete_details
      ) {
        responsesError("response_incomplete");
      }
    } else if (event.type === "response.output_item.added") this.add(event);
    else if (event.type === "response.output_item.done") this.done(event);
    else if (event.type === "response.function_call_arguments.delta")
      this.argumentsDelta(event);
    else if (event.type === "response.function_call_arguments.done")
      this.argumentsDone(event);
    else if (
      event.type === "response.output_text.delta" ||
      event.type === "response.refusal.delta"
    ) {
      return this.textDelta(event);
    } else if (
      event.type === "response.output_text.done" ||
      event.type === "response.refusal.done"
    )
      this.textDone(event);
    else this.auxiliary(event);
    return undefined;
  }

  private captureUsage(event: Record<string, unknown>): void {
    if (
      [
        "response.completed",
        "response.failed",
        "response.incomplete",
        "response.in_progress",
      ].includes(String(event.type))
    ) {
      const response = object(event.response);
      if (response.usage) this.observedUsage = object(response.usage);
    }
  }

  finish(tools: AiToolDefinition[] = []): ResponsesResult {
    if (!this.terminal) responsesError("stream_incomplete");
    if (
      !Array.isArray(this.terminal.output) ||
      this.terminal.output.length > MAX_RESPONSES_ITEMS
    ) {
      responsesError("malformed_response");
    }
    const output = this.terminal.output.map(object);
    this.reconcile(output);
    const calls: AiToolCall[] = [];
    const ids = new Set<string>();
    let content = "";
    for (const item of output) {
      validateOutput(item);
      if (item.type === "message") content += messageText(item);
      else if (item.type === "function_call") {
        const call = responseToolCall(item, tools);
        if (ids.has(call.id)) responsesError("duplicate_tool_call_id");
        ids.add(call.id);
        calls.push(call);
      }
    }
    if (!content.startsWith(this.emitted))
      responsesError("stream_output_mismatch");
    return {
      output,
      content,
      tools: calls,
      ...(this.terminal.usage ? { usage: object(this.terminal.usage) } : {}),
    };
  }

  private auxiliary(event: Record<string, unknown>): void {
    const type = String(event.type);
    if (type === "response.created" || type === "response.in_progress") {
      const response = object(event.response);
      idOf(response.id);
      if (response.status !== "in_progress")
        responsesError("malformed_response");
      return;
    }
    if (
      type === "response.content_part.added" ||
      type === "response.content_part.done"
    ) {
      const item = this.lookup(event);
      if (item.item.type !== "message" || item.done)
        responsesError("output_item_mismatch");
      indexOf(event.content_index);
      const part = object(event.part);
      if (!["output_text", "refusal"].includes(String(part.type)))
        responsesError("unsupported_output");
      return;
    }
    const reasoningTypes = [
      "response.reasoning_summary_part.added",
      "response.reasoning_summary_part.done",
      "response.reasoning_summary_text.delta",
      "response.reasoning_summary_text.done",
      "response.reasoning_text.delta",
      "response.reasoning_text.done",
    ];
    if (!reasoningTypes.includes(type))
      responsesError("unsupported_stream_event");
    const item = this.lookup(event);
    if (item.item.type !== "reasoning" || item.done)
      responsesError("output_item_mismatch");
    indexOf(
      type.includes("summary") ? event.summary_index : event.content_index,
    );
    if (type.includes("_part.")) {
      const part = object(event.part);
      if (part.type !== "summary_text" || typeof part.text !== "string")
        responsesError("malformed_response");
    } else if (
      typeof (type.endsWith(".delta") ? event.delta : event.text) !== "string"
    ) {
      responsesError("malformed_response");
    }
  }

  private add(event: Record<string, unknown>): void {
    const item = object(event.item);
    const index = indexOf(event.output_index);
    const id = idOf(item.id);
    if (!["message", "function_call", "reasoning"].includes(String(item.type)))
      responsesError("unsupported_output");
    if (
      this.byIndex.has(index) ||
      this.byId.has(id) ||
      this.byIndex.size >= MAX_RESPONSES_ITEMS
    ) {
      responsesError("duplicate_output_item");
    }
    const state: ItemState = {
      id,
      index,
      item,
      arguments: "",
      texts: new Map(),
      done: false,
    };
    this.byIndex.set(index, state);
    this.byId.set(id, state);
  }

  private lookup(event: Record<string, unknown>): ItemState {
    const item = this.byId.get(idOf(event.item_id));
    if (!item || item.index !== indexOf(event.output_index))
      responsesError("output_item_mismatch");
    return item;
  }

  private done(event: Record<string, unknown>): void {
    const final = object(event.item);
    const item = this.lookup({ ...event, item_id: final.id });
    if (item.done || item.item.type !== final.type)
      responsesError("output_item_mismatch");
    this.compare(item, final);
    item.item = final;
    item.done = true;
  }

  private argumentsDelta(event: Record<string, unknown>): void {
    const item = this.lookup(event);
    if (
      item.item.type !== "function_call" ||
      item.done ||
      typeof event.delta !== "string"
    ) {
      responsesError("malformed_response");
    }
    item.arguments += event.delta;
    boundedJson(item.arguments);
  }

  private argumentsDone(event: Record<string, unknown>): void {
    const item = this.lookup(event);
    if (
      item.item.type !== "function_call" ||
      item.done ||
      typeof event.arguments !== "string" ||
      !event.arguments.startsWith(item.arguments)
    )
      responsesError("stream_output_mismatch");
    item.arguments = event.arguments;
  }

  private textDelta(event: Record<string, unknown>): string {
    const item = this.lookup(event);
    if (
      item.item.type !== "message" ||
      item.done ||
      typeof event.delta !== "string"
    )
      responsesError("malformed_response");
    const index = indexOf(event.content_index);
    item.texts.set(index, (item.texts.get(index) ?? "") + event.delta);
    this.emitted += event.delta;
    boundedJson(this.emitted);
    return event.delta;
  }

  private textDone(event: Record<string, unknown>): void {
    const item = this.lookup(event);
    const index = indexOf(event.content_index);
    const text =
      event.type === "response.refusal.done" ? event.refusal : event.text;
    if (
      typeof text !== "string" ||
      !text.startsWith(item.texts.get(index) ?? "")
    )
      responsesError("stream_output_mismatch");
    item.texts.set(index, text);
  }

  private compare(state: ItemState, final: Record<string, unknown>): void {
    if (state.id !== final.id || state.item.type !== final.type)
      responsesError("output_item_mismatch");
    if (final.type === "function_call") {
      for (const key of ["call_id", "name", "namespace"]) {
        if (state.item[key] !== final[key])
          responsesError("output_item_mismatch");
      }
      if (
        typeof final.arguments !== "string" ||
        !final.arguments.startsWith(state.arguments)
      )
        responsesError("stream_output_mismatch");
      if (state.done && state.item.arguments !== final.arguments)
        responsesError("stream_output_mismatch");
    }
    if (final.type === "message") {
      const content = Array.isArray(final.content) ? final.content : [];
      for (const [index, text] of state.texts) {
        const part = object(content[index]);
        const value = part.type === "refusal" ? part.refusal : part.text;
        if (typeof value !== "string" || !value.startsWith(text))
          responsesError("stream_output_mismatch");
      }
      if (state.done && messageText(state.item) !== messageText(final))
        responsesError("stream_output_mismatch");
    }
    if (
      state.done &&
      final.type === "reasoning" &&
      state.item.encrypted_content !== final.encrypted_content
    ) {
      responsesError("stream_output_mismatch");
    }
  }

  private reconcile(output: Record<string, unknown>[]): void {
    const seen = new Set<string>();
    output.forEach((item, index) => {
      const id = idOf(item.id);
      if (seen.has(id)) responsesError("duplicate_output_item");
      seen.add(id);
      const state = this.byIndex.get(index);
      if (state) this.compare(state, item);
      if (this.byId.has(id) && this.byId.get(id)?.index !== index)
        responsesError("output_item_mismatch");
    });
    if ([...this.byId.keys()].some((id) => !seen.has(id)))
      responsesError("missing_output_item");
  }
}

function indexOf(value: unknown): number {
  if (
    typeof value !== "number" ||
    !Number.isSafeInteger(value) ||
    value < 0 ||
    value >= MAX_RESPONSES_ITEMS
  ) {
    return responsesError("malformed_response");
  }
  return value;
}

function idOf(value: unknown): string {
  if (typeof value !== "string" || !value)
    return responsesError("malformed_response");
  return value;
}

function messageText(item: Record<string, unknown>): string {
  if (item.role !== "assistant" || !Array.isArray(item.content))
    return responsesError("malformed_response");
  return item.content
    .map((value: unknown) => {
      const part = object(value);
      const text =
        part.type === "output_text"
          ? part.text
          : part.type === "refusal"
            ? part.refusal
            : undefined;
      if (typeof text !== "string") return responsesError("unsupported_output");
      return text;
    })
    .join("");
}

function validateOutput(item: Record<string, unknown>): void {
  if (item.status !== undefined && item.status !== "completed")
    responsesError("response_incomplete");
  if (!["message", "function_call", "reasoning"].includes(String(item.type)))
    responsesError("unsupported_output");
  if (
    item.type === "reasoning" &&
    (typeof item.encrypted_content !== "string" || !item.encrypted_content)
  ) {
    responsesError("continuation_required");
  }
}

export function classifyResponseError(value: unknown): string {
  if (
    value === "subscription_sharing_usage_limit_exceeded" ||
    value === "subscription_sharing_usage_unavailable"
  ) {
    return "usage_limit";
  }
  if (
    value === "subscription_sharing_invalid_user" ||
    value === "invalid_api_key" ||
    value === "token_expired" ||
    value === "authentication_error"
  )
    return "authentication_required";
  if (
    value === "chatpass_v2_scope_not_authorized" ||
    value === "chatpass_v2_invalid_authorization_context" ||
    value === "subscription_sharing_consent_required" ||
    value === "consent_required" ||
    value === "insufficient_scope"
  ) {
    return "consent_required";
  }
  if (value === "subscription_sharing_user_not_eligible")
    return "permission_denied";
  if (value === "subscription_sharing_unsupported_capability")
    return "unsupported_parameter";
  if (value === "subscription_sharing_route_not_supported")
    return "unsupported_route";
  return "provider_error";
}
