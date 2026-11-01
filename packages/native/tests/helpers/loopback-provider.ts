/**
 * Loopback OpenAI-compatible provider fixtures for the native Rust provider
 * lane: canned `chat/completions` and `responses` SSE responders with
 * request capture, mirroring the wire shapes the Rust presets produce
 * (rust/crates/core/src/providers/{chat_completions,responses}_request.rs)
 * and the TypeScript reference client tests pin. Shared by the native
 * package tests.
 */

export interface CapturedRequest {
  path: string;
  headers: Record<string, string>;
  body: Record<string, unknown>;
}

export interface LoopbackProvider {
  baseUrl: string;
  /** Every POSTed request body with its path and lower-cased headers. */
  requests: CapturedRequest[];
  stop(close?: boolean): Promise<void>;
}

export interface ChatCompletionCanned {
  /** Streamed content deltas; joined they form the final message. */
  deltas?: string[];
  /** Streamed reasoning_content deltas (deepseek-style). */
  reasoningDeltas?: string[];
  /** Final usage object exactly as an OpenAI-compatible server reports it. */
  usage?: unknown;
}

export interface ResponsesCanned {
  deltas?: string[];
  reasoningDeltas?: string[];
  /** The `usage` object inside the `response.completed` payload. */
  usage?: unknown;
  /** Non-empty encrypted content on the reasoning item (the request
   * includes `reasoning.encrypted_content`, so reasoning without it is a
   * protocol violation the provider must reject). */
  encryptedContent?: string;
}

export interface LoopbackOptions {
  chat?: ChatCompletionCanned;
  responses?: ResponsesCanned;
}

function sseEvent(value: Record<string, unknown>): string {
  return `data: ${JSON.stringify(value)}\n\n`;
}

function chatSse(canned: ChatCompletionCanned): string {
  const chunks: string[] = [];
  let id = 0;
  const chunk = (
    delta: Record<string, unknown>,
    finishReason: string | null,
  ) => {
    id += 1;
    chunks.push(
      `data: ${JSON.stringify({
        id: `chatcmpl-${id}`,
        object: "chat.completion.chunk",
        created: 1,
        model: "loopback-model",
        choices: [{ index: 0, delta, finish_reason: finishReason }],
      })}\n\n`,
    );
  };
  chunk({ role: "assistant" }, null);
  for (const delta of canned.reasoningDeltas ?? []) {
    chunk({ reasoning_content: delta }, null);
  }
  for (const delta of canned.deltas ?? ["Hello world"]) {
    chunk({ content: delta }, null);
  }
  chunk({}, "stop");
  if (canned.usage !== undefined) {
    id += 1;
    chunks.push(
      `data: ${JSON.stringify({
        id: `chatcmpl-${id}`,
        object: "chat.completion.chunk",
        created: 1,
        model: "loopback-model",
        choices: [],
        usage: canned.usage,
      })}\n\n`,
    );
  }
  chunks.push("data: [DONE]\n\n");
  return chunks.join("");
}

function responsesSse(canned: ResponsesCanned, stage: number): string {
  const deltas = canned.deltas ?? ["Hello world"];
  const frames: string[] = [
    sseEvent({
      type: "response.created",
      response: { id: `resp_loopback_${stage}`, status: "in_progress" },
    }),
  ];
  const output: Record<string, unknown>[] = [];
  const reasoningDeltas = stage === 0 ? (canned.reasoningDeltas ?? []) : [];
  const messageDeltas = reasoningDeltas.length > 0 && stage === 0 ? [] : deltas;
  let outputIndex = 0;
  if (reasoningDeltas.length > 0) {
    frames.push(
      sseEvent({
        type: "response.output_item.added",
        output_index: outputIndex,
        item: { id: "reasoning1", type: "reasoning" },
      }),
    );
    for (const delta of reasoningDeltas) {
      frames.push(
        sseEvent({
          type: "response.reasoning_text.delta",
          output_index: outputIndex,
          item_id: "reasoning1",
          content_index: 0,
          delta,
        }),
      );
    }
    output.push({
      id: "reasoning1",
      type: "reasoning",
      encrypted_content: canned.encryptedContent ?? "enc_loopback",
    });
    outputIndex += 1;
  }
  if (messageDeltas.length > 0) {
    frames.push(
      sseEvent({
        type: "response.output_item.added",
        output_index: outputIndex,
        item: {
          id: "message1",
          type: "message",
          role: "assistant",
          content: [],
        },
      }),
    );
    for (const delta of messageDeltas) {
      frames.push(
        sseEvent({
          type: "response.output_text.delta",
          output_index: outputIndex,
          item_id: "message1",
          content_index: 0,
          delta,
        }),
      );
    }
    output.push({
      id: "message1",
      type: "message",
      role: "assistant",
      status: "completed",
      content: [{ type: "output_text", text: deltas.join("") }],
    });
  }
  frames.push(
    sseEvent({
      type: "response.completed",
      response: {
        status: "completed",
        output,
        ...(canned.usage === undefined ? {} : { usage: canned.usage }),
      },
    }),
  );
  frames.push("data: [DONE]\n\n");
  return frames.join("");
}

const DEFAULT_CHAT_USAGE = {
  prompt_tokens: 9,
  completion_tokens: 3,
  total_tokens: 12,
  prompt_tokens_details: { cached_tokens: 3 },
  completion_tokens_details: { reasoning_tokens: 1 },
};

const DEFAULT_RESPONSES_USAGE = {
  input_tokens: 7,
  output_tokens: 3,
  total_tokens: 10,
  input_tokens_details: { cached_tokens: 3 },
  output_tokens_details: { reasoning_tokens: 1 },
};

/** Starts a loopback server answering POST /chat/completions and
 * POST /responses with SSE. Binds 127.0.0.1 on an ephemeral port. */
export async function startProviderMock(
  options: LoopbackOptions = {},
): Promise<LoopbackProvider> {
  const requests: CapturedRequest[] = [];
  let responsesStage = 0;
  const chat = options.chat ?? {
    deltas: ["Hel", "lo", " world"],
    reasoningDeltas: ["thinking"],
    usage: DEFAULT_CHAT_USAGE,
  };
  const responses = options.responses ?? {
    deltas: ["Hel", "lo", " world"],
    reasoningDeltas: ["thinking"],
    usage: DEFAULT_RESPONSES_USAGE,
  };
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      if (request.method !== "POST") {
        return new Response("method not allowed", { status: 405 });
      }
      const headers: Record<string, string> = {};
      request.headers.forEach((value, key) => {
        headers[key.toLowerCase()] = value;
      });
      const raw = await request.text();
      let body: Record<string, unknown> = {};
      try {
        body = JSON.parse(raw) as Record<string, unknown>;
      } catch {
        return new Response("invalid json", { status: 400 });
      }
      if (
        url.pathname !== "/chat/completions" &&
        url.pathname !== "/responses"
      ) {
        return new Response("not found", { status: 404 });
      }
      requests.push({ path: url.pathname, headers, body });
      let sse: string;
      if (url.pathname === "/chat/completions") {
        sse = chatSse(chat);
      } else {
        const stage = responsesStage;
        responsesStage += 1;
        sse = responsesSse(responses, stage);
      }
      return new Response(sse, {
        headers: { "content-type": "text/event-stream" },
      });
    },
  });
  return {
    baseUrl: `http://127.0.0.1:${server.port}`,
    requests,
    async stop(close = true): Promise<void> {
      server.stop(close);
    },
  };
}
