import { nowIso } from "../ids.js";
import { parseSseStream } from "./sse.js";
import type { AiChatRequest, AiProviderClient } from "./client.js";
import type {
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
  AiToolCall,
  AiToolDefinition,
} from "@agentkit/contracts";

export interface OpenAiCompatibleClientOptions {
  id: string;
  kind: AiProviderKind;
  baseUrl: string;
  apiKey?: string;
  /**
   * Static headers sent on every request to this provider (merged after the
   * built-in accept/authorization headers, so they cannot override auth). Used
   * by the OpenPCB Cloud provider to attribute calls to a workspace + run
   * (`x-openpcb-workspace-id`, required by the metered proxy; `x-openpcb-run-id`
   * for usage stitching). The client is built per run, so per-run values ride here.
   */
  extraHeaders?: Record<string, string>;
  /** Override fetch for tests. */
  fetchImpl?: typeof fetch;
}

export class OpenAiCompatibleClient implements AiProviderClient {
  readonly id: string;
  readonly kind: AiProviderKind;
  private readonly baseUrl: string;
  private readonly apiKey?: string;
  private readonly extraHeaders?: Record<string, string>;
  private readonly fetchImpl: typeof fetch;

  constructor(options: OpenAiCompatibleClientOptions) {
    this.id = options.id;
    this.kind = options.kind;
    this.baseUrl = stripTrailingSlash(options.baseUrl);
    this.apiKey = options.apiKey;
    this.extraHeaders = options.extraHeaders;
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch.bind(globalThis);
  }

  static fromConfig(
    config: AiProviderConfig,
    fetchImpl?: typeof fetch,
  ): OpenAiCompatibleClient {
    return new OpenAiCompatibleClient({
      id: config.id,
      kind: config.kind,
      baseUrl: config.baseUrl,
      apiKey: config.apiKey,
      fetchImpl,
    });
  }

  async capabilities(
    signal?: AbortSignal,
    model?: string,
  ): Promise<AiProviderCapabilities> {
    const checkedAt = nowIso();
    let modelList = false;
    let warning: string | undefined;
    try {
      const models = await this.listModels(signal);
      modelList = models.length > 0;
    } catch (err) {
      warning = `Model list failed: ${errMsg(err)}`;
    }
    let toolCalling = false;
    let streaming = true;
    try {
      const probe = await this.probeToolCall(signal, model);
      toolCalling = probe.toolCalling;
      streaming = probe.streaming;
      if (probe.warning && !warning) warning = probe.warning;
    } catch (err) {
      if (!warning) warning = `Capability probe failed: ${errMsg(err)}`;
    }
    return {
      streaming,
      toolCalling,
      modelList,
      checkedAt,
      warning,
    };
  }

  async listModels(signal?: AbortSignal): Promise<AiProviderModel[]> {
    const response = await this.fetchImpl(`${this.baseUrl}/models`, {
      method: "GET",
      headers: this.buildHeaders(),
      signal,
    });
    if (!response.ok) {
      throw new Error(
        `GET /models -> ${response.status}: ${await safeBody(response)}`,
      );
    }
    const payload = (await response.json()) as {
      data?: Array<{ id?: string; name?: string }>;
    };
    const fetchedAt = nowIso();
    return (payload.data ?? [])
      .filter(
        (m): m is { id: string; name?: string } => typeof m.id === "string",
      )
      .map((m) => ({
        providerId: this.id,
        modelId: m.id,
        displayName: m.name ?? null,
        fetchedAt,
      }));
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const body = this.buildRequestBody(input, true);
    const runId = input.runId;
    yield {
      type: "run.started",
      runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    };

    let response: Response;
    try {
      response = await this.postChatCompletions(body, input.signal);
    } catch (err) {
      if (input.signal?.aborted) {
        yield {
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: errMsg(err) },
        };
        return;
      }
      yield {
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage: errMsg(err) },
      };
      return;
    }

    if (!response.ok) {
      const errorMessage = `POST /chat/completions -> ${response.status}: ${await safeBody(response)}`;
      yield {
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage, errorCode: String(response.status) },
      };
      return;
    }

    if (!response.body) {
      yield {
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage: "Provider returned empty body" },
      };
      return;
    }

    let aggregatedContent = "";
    let aggregatedReasoning = "";
    let droppedSseLines = 0;
    const toolCallAccumulators = new Map<number, ToolCallAccumulator>();
    let finishReason: string | undefined;

    try {
      for await (const line of parseSseStream(response.body, input.signal)) {
        let event: OpenAiStreamChunk | null = null;
        try {
          event = JSON.parse(line.data) as OpenAiStreamChunk;
        } catch {
          droppedSseLines += 1;
          continue;
        }
        const choice = event.choices?.[0];
        if (!choice) continue;
        const delta = choice.delta ?? {};
        if (typeof delta.content === "string" && delta.content.length > 0) {
          aggregatedContent += delta.content;
          yield {
            type: "run.message.delta",
            runId,
            timestamp: nowIso(),
            data: { delta: delta.content },
          };
        }
        // Reasoning models emit chain-of-thought into a separate field; accumulate it
        // (do NOT merge into the visible answer) so empty-content turns can surface it.
        const reasoningDelta = delta.reasoning_content ?? delta.reasoning;
        if (typeof reasoningDelta === "string" && reasoningDelta.length > 0) {
          aggregatedReasoning += reasoningDelta;
        }
        if (delta.tool_calls) {
          for (const tcDelta of delta.tool_calls) {
            const idx = typeof tcDelta.index === "number" ? tcDelta.index : 0;
            let acc = toolCallAccumulators.get(idx);
            if (!acc) {
              acc = { id: "", name: "", argumentsJson: "" };
              toolCallAccumulators.set(idx, acc);
            }
            if (tcDelta.id) acc.id = tcDelta.id;
            const fn = tcDelta.function;
            if (fn) {
              if (typeof fn.name === "string" && fn.name.length > 0)
                acc.name = fn.name;
              if (typeof fn.arguments === "string")
                acc.argumentsJson += fn.arguments;
            }
          }
        }
        if (choice.finish_reason) {
          finishReason = choice.finish_reason;
        }
      }
    } catch (err) {
      if (input.signal?.aborted) {
        yield {
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: errMsg(err) },
        };
        return;
      }
      yield {
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage: errMsg(err) },
      };
      return;
    }

    const toolCalls: AiToolCall[] = Array.from(toolCallAccumulators.entries())
      .sort(([a], [b]) => a - b)
      .map(([, acc], i) => ({
        id: acc.id || `call_${i}`,
        name: acc.name,
        argumentsJson: acc.argumentsJson || "{}",
      }))
      .filter((tc) => tc.name.length > 0);

    // Surface malformed-SSE drops only when they likely cost us the answer, to avoid
    // noise on healthy streams.
    if (
      droppedSseLines > 0 &&
      aggregatedContent.length === 0 &&
      toolCalls.length === 0
    ) {
      yield {
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "sse_parse",
          message: `Dropped ${droppedSseLines} malformed SSE line(s); response may be incomplete.`,
        },
      };
    }

    // finish_reason=tool_calls but no reconstructable call (truncated/garbled tool
    // block) would otherwise read as an empty success. Warn so the loop doesn't
    // silently end an iteration with nothing to act on.
    if (finishReason === "tool_calls" && toolCalls.length === 0) {
      yield {
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "tool_call_cap",
          message:
            "Provider reported finish_reason=tool_calls but emitted no usable tool call.",
        },
      };
    }

    yield {
      type: "run.message.completed",
      runId,
      timestamp: nowIso(),
      data: {
        content: aggregatedContent,
        toolCallCount: toolCalls.length,
        toolCalls,
        reasoningContent: aggregatedReasoning || undefined,
        finishReason,
      },
    };

    for (const tc of toolCalls) {
      yield {
        type: "run.tool.requested",
        runId,
        timestamp: nowIso(),
        data: {
          toolCallId: tc.id,
          toolName: tc.name,
          argumentsJson: tc.argumentsJson,
        },
      };
    }
  }

  /**
   * POST /chat/completions with a `max_completion_tokens` fallback. Newer servers
   * (and some proxies) reject the legacy `max_tokens` field with a 400; when the body
   * mentions it, retry once translating `max_tokens` → `max_completion_tokens`.
   */
  private async postChatCompletions(
    body: Record<string, unknown>,
    signal?: AbortSignal,
  ): Promise<Response> {
    const send = (b: Record<string, unknown>): Promise<Response> =>
      this.fetchImpl(`${this.baseUrl}/chat/completions`, {
        method: "POST",
        headers: { ...this.buildHeaders(), "content-type": "application/json" },
        body: JSON.stringify(b),
        signal,
      });

    const response = await send(body);
    if (
      response.ok ||
      response.status !== 400 ||
      !("max_tokens" in body) ||
      "max_completion_tokens" in body
    ) {
      return response;
    }
    const detail = await safeBody(response);
    if (!/max_tokens|max_completion_tokens/i.test(detail)) {
      // Unrelated 400; rebuild a response so the caller can still read the body.
      return new Response(detail, {
        status: response.status,
        statusText: response.statusText,
        headers: response.headers,
      });
    }
    const { max_tokens, ...rest } = body;
    return send({ ...rest, max_completion_tokens: max_tokens });
  }

  /**
   * Probe whether the provider supports tool-calling by sending a tiny tool-enabled completion.
   * Returns toolCalling=true on a 2xx response that emits a tool_call. Truncated probes
   * (finish_reason "length" with no tool_call) are treated as INCONCLUSIVE → toolCalling=true,
   * so reasoning models (which spend the budget on reasoning_content) are never hard-disabled.
   */
  private async probeToolCall(
    signal?: AbortSignal,
    model?: string,
  ): Promise<{ toolCalling: boolean; streaming: boolean; warning?: string }> {
    const tools: AiToolDefinition[] = [
      {
        name: "echo",
        version: "1",
        effect: "read",
        capability: "probe",
        description:
          "Echo the provided text. Used only for capability probing.",
        inputSchema: {
          type: "object",
          properties: { text: { type: "string" } },
          required: ["text"],
        },
      },
    ];
    const body = this.buildRequestBody(
      {
        runId: "probe",
        model: model ?? "",
        messages: [
          { role: "user", content: 'Call the echo tool with text "ok".' },
        ],
        tools,
        // Reasoning models spend the first tokens on reasoning_content; a tiny budget
        // (was 16) truncates before any tool_call and falsely reports no tool support.
        maxOutputTokens: 256,
      },
      false,
    );
    if (!model) delete (body as { model?: unknown }).model;
    try {
      const response = await this.fetchImpl(
        `${this.baseUrl}/chat/completions`,
        {
          method: "POST",
          headers: {
            ...this.buildHeaders(),
            "content-type": "application/json",
          },
          body: JSON.stringify(body),
          signal,
        },
      );
      if (!response.ok) {
        return {
          toolCalling: false,
          streaming: true,
          warning: `Probe HTTP ${response.status}`,
        };
      }
      const json = (await response.json()) as {
        choices?: Array<{
          message?: { tool_calls?: unknown[] };
          finish_reason?: string;
        }>;
      };
      const choice = json.choices?.[0];
      const toolCalls = choice?.message?.tool_calls;
      const hasToolCalls = Array.isArray(toolCalls) && toolCalls.length > 0;
      // Truncation is inconclusive, not a negative: don't poison the capability cache.
      const truncated = choice?.finish_reason === "length";
      if (hasToolCalls) return { toolCalling: true, streaming: true };
      if (truncated) {
        return {
          toolCalling: true,
          streaming: true,
          warning:
            "Tool-call probe truncated (finish_reason=length); assuming tools are supported.",
        };
      }
      return {
        toolCalling: false,
        streaming: true,
      };
    } catch (err) {
      return {
        toolCalling: false,
        streaming: true,
        warning: `Probe failed: ${errMsg(err)}`,
      };
    }
  }

  private buildRequestBody(
    input: AiChatRequest,
    stream: boolean,
  ): Record<string, unknown> {
    const body: Record<string, unknown> = {
      model: input.model,
      messages: input.messages.map((m) => {
        const out: Record<string, unknown> = {
          role: m.role,
          content: m.content,
        };
        if (m.name) out.name = m.name;
        if (m.toolCallId) out.tool_call_id = m.toolCallId;
        if (m.toolCalls && m.toolCalls.length > 0) {
          out.tool_calls = m.toolCalls.map((tc) => ({
            id: tc.id,
            type: "function",
            function: { name: tc.name, arguments: tc.argumentsJson },
          }));
        }
        return out;
      }),
      stream,
    };
    if (input.temperature !== undefined) body.temperature = input.temperature;
    if (input.maxOutputTokens !== undefined)
      body.max_tokens = input.maxOutputTokens;
    if (input.tools && input.tools.length > 0) {
      body.tools = input.tools.map((t) => ({
        type: "function",
        function: {
          name: t.name,
          description: t.description,
          parameters: t.inputSchema,
        },
      }));
      body.tool_choice = "auto";
    }
    return body;
  }

  private buildHeaders(): Record<string, string> {
    // extraHeaders first so the built-in accept/auth headers below always win.
    const headers: Record<string, string> = { ...this.extraHeaders };
    headers.accept = "application/json";
    if (this.apiKey) headers.authorization = `Bearer ${this.apiKey}`;
    if (this.kind === "openrouter") {
      // Optional attribution headers for OpenRouter's app leaderboard (no-op elsewhere).
      headers["HTTP-Referer"] = "https://openpcb.app";
      headers["X-Title"] = "OpenPCB";
    }
    return headers;
  }
}

interface ToolCallAccumulator {
  id: string;
  name: string;
  argumentsJson: string;
}

interface OpenAiStreamChunk {
  choices?: Array<{
    delta?: {
      content?: string;
      role?: string;
      /** Reasoning-model chain-of-thought (vLLM/oMLX/DeepSeek style). */
      reasoning_content?: string;
      /** Alternate field name used by some providers. */
      reasoning?: string;
      tool_calls?: Array<{
        index?: number;
        id?: string;
        type?: string;
        function?: { name?: string; arguments?: string };
      }>;
    };
    finish_reason?: string;
  }>;
}

function stripTrailingSlash(url: string): string {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

async function safeBody(response: Response): Promise<string> {
  try {
    return await response.text();
  } catch {
    return "<unreadable body>";
  }
}
