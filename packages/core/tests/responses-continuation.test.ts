import { describe, expect, it } from "bun:test";
import {
  ResponsesClient,
  type AiProviderContinuation,
  type TrustedResponsesTransport,
} from "../src/index.js";
import {
  call,
  collect,
  encode,
  harness,
  message,
  request,
  stream,
  terminal,
} from "./responses-helpers.js";

const scope = { chatId: "chat", branchId: "branch" };
const reasoning = {
  type: "reasoning",
  id: "reasoning",
  encrypted_content: "opaque-secret",
  summary: [],
};
function failure(
  events: Awaited<ReturnType<typeof collect>>,
): string | undefined {
  const event = events.find((event) => event.type === "run.failed");
  return event?.type === "run.failed" ? event.data.errorCode : undefined;
}
async function firstPass(): Promise<AiProviderContinuation> {
  let state: AiProviderContinuation | undefined;
  const events = await collect(
    harness([terminal([reasoning, call("one")])]).client,
    {
      ...request,
      continuationScope: scope,
      onContinuation(value) {
        state = value;
      },
    },
  );
  expect(failure(events)).toBeUndefined();
  if (!state) throw new Error("Missing continuation");
  return state;
}
function nextMessages(): typeof request.messages {
  return [
    ...request.messages,
    {
      role: "assistant",
      content: "",
      toolCalls: [{ id: "one", name: "lookup", argumentsJson: "{}" }],
    },
    { role: "tool", content: "result", toolCallId: "one", name: "lookup" },
  ];
}

describe("Responses continuation", () => {
  it("replays encrypted state, exact call and result without duplicate assistant", async () => {
    const previous = await firstPass();
    const { client, sent } = harness();
    let current: AiProviderContinuation | undefined;
    const events = await collect(client, {
      ...request,
      messages: nextMessages(),
      continuation: previous,
      continuationScope: scope,
      onContinuation(value) {
        current = value;
      },
    });
    expect(failure(events)).toBeUndefined();
    expect(sent[0]?.body?.input).toEqual([
      {
        type: "message",
        role: "user",
        content: [{ type: "input_text", text: "Hello" }],
      },
      reasoning,
      call("one"),
      { type: "function_call_output", call_id: "one", output: "result" },
    ]);
    expect(current?.messageCount).toBe(4);
    expect(current?.inputItems.at(-1)).toEqual(message("Hi"));
    expect(JSON.stringify(events)).not.toContain("opaque-secret");
  });

  it.each([
    "providerId",
    "connectionId",
    "generation",
    "chatId",
    "branchId",
    "model",
    "protocol",
  ] as const)("scope mismatch %s rejected before network", async (field) => {
    const previous = await firstPass();
    const scopeValue = {
      ...previous.scope,
      [field]: field === "generation" ? 2 : "different",
    };
    const { client, sent } = harness();
    const events = await collect(client, {
      ...request,
      messages: nextMessages(),
      continuationScope: scope,
      continuation: {
        ...previous,
        scope: scopeValue,
      } as AiProviderContinuation,
    });
    expect(failure(events)).toBeDefined();
    expect(sent).toHaveLength(0);
  });

  it("changed history rejected; metadata and tool-result names do not change digest", async () => {
    const previous = await firstPass();
    const { client, sent } = harness();
    const changed = nextMessages();
    changed[0] = { role: "user", content: "Different" };
    expect(
      failure(
        await collect(client, {
          ...request,
          messages: changed,
          continuationScope: scope,
          continuation: previous,
        }),
      ),
    ).toBe("continuation_history_mismatch");
    expect(sent).toHaveLength(0);
    const metadata = nextMessages().map((value) => ({
      ...value,
      metadata: { host: true },
    }));
    expect(
      failure(
        await collect(client, {
          ...request,
          messages: metadata,
          continuationScope: scope,
          continuation: previous,
        }),
      ),
    ).toBeUndefined();
  });

  it("required state absence, wrong version, oversize state and missing opaque replay fail closed", async () => {
    const previous = await firstPass();
    const cases = [
      undefined,
      { ...previous, version: 2 },
      { ...previous, messageDigest: "x".repeat(1024 * 1024) },
      {
        ...previous,
        inputItems: [{ type: "reasoning", encrypted_content: undefined }],
      },
    ];
    for (const continuation of cases) {
      const { client, sent } = harness();
      expect(
        failure(
          await collect(client, {
            ...request,
            messages: nextMessages(),
            continuationRequired: true,
            continuationScope: scope,
            continuation: continuation as AiProviderContinuation | undefined,
          }),
        ),
      ).toBeDefined();
      expect(sent).toHaveLength(0);
    }
  });

  it("no callback or scope cannot silently drop required reasoning", async () => {
    expect(
      failure(await collect(harness([terminal([reasoning])]).client)),
    ).toBe("continuation_scope_required");
    expect(
      failure(
        await collect(harness([terminal([reasoning])]).client, {
          ...request,
          continuationScope: scope,
        }),
      ),
    ).toBe("continuation_callback_required");
  });

  it("async persistence completes before any tool dispatch; persistence failure sanitized", async () => {
    let release: (() => void) | undefined;
    let called: (() => void) | undefined;
    const callbackStarted = new Promise<void>((resolve) => {
      called = resolve;
    });
    const persisted = new Promise<void>((resolve) => {
      release = resolve;
    });
    const { client } = harness([terminal([reasoning, call("one")])]);
    const iterator = client
      .streamChat({
        ...request,
        continuationScope: scope,
        async onContinuation() {
          called?.();
          await persisted;
        },
      })
      [Symbol.asyncIterator]();
    expect((await iterator.next()).value?.type).toBe("run.started");
    let nextDone = false;
    const next = iterator.next().then((value) => {
      nextDone = true;
      return value;
    });
    await callbackStarted;
    expect(nextDone).toBe(false);
    release?.();
    expect((await next).value?.type).toBe("run.usage");
    await iterator.return?.();
    const events = await collect(client, {
      ...request,
      continuationScope: scope,
      onContinuation() {
        throw new Error("token=secret opaque-secret");
      },
    });
    expect(failure(events)).toBe("transport_error");
    expect(events.some((event) => event.type === "run.tool.requested")).toBe(
      false,
    );
    expect(JSON.stringify(events)).not.toContain("secret");
  });

  it("failed final usage cannot be persisted as successful continuation", async () => {
    let persisted = false;
    const final = terminal([reasoning]);
    const response = final.response as Record<string, unknown>;
    response.usage = { input_tokens: -1 };
    const events = await collect(harness([final]).client, {
      ...request,
      continuationScope: scope,
      onContinuation() {
        persisted = true;
      },
    });
    expect(failure(events)).toBe("malformed_usage");
    expect(persisted).toBe(false);
  });
});

describe("Responses transport and catalog", () => {
  it("account catalogs preserve server order/visibility and never share cache", async () => {
    const clients = ["A", "B"].map((connectionId) => {
      const { transport } = harness(undefined, "chatgpt-account");
      const scoped: TrustedResponsesTransport = {
        ...transport,
        identity: { ...transport.identity, connectionId },
        async request(input) {
          expect(input.operation).toBe("models");
          return Response.json({
            models: [
              {
                slug: `${connectionId}-z`,
                display_name: "Z",
                visibility: "list",
              },
              { slug: "hidden", display_name: "Hidden", visibility: "hide" },
              {
                slug: `${connectionId}-a`,
                display_name: "A",
                visibility: "list",
              },
            ],
          });
        },
      };
      return new ResponsesClient({ id: "provider", transport: scoped });
    });
    expect(
      (await clients[0]?.listModels())?.map((model) => model.modelId),
    ).toEqual(["A-z", "A-a"]);
    expect(
      (await clients[1]?.listModels())?.map((model) => model.modelId),
    ).toEqual(["B-z", "B-a"]);
  });

  it("API catalog uses data; immutable connection snapshot rejects rebinding", async () => {
    const { transport } = harness();
    transport.request = async () =>
      Response.json({ data: [{ id: "model", name: "Model" }] });
    const client = new ResponsesClient({ id: "provider", transport });
    expect(Object.isFrozen(client.connectionIdentity)).toBe(true);
    expect((await client.listModels())[0]?.displayName).toBe("Model");
    expect(() => new ResponsesClient({ id: "other", transport })).toThrow(
      "invalid_connection",
    );
  });

  it("transport connection drift cannot dispatch under stale scope", async () => {
    const { transport } = harness();
    let calls = 0;
    transport.request = async (input) => {
      calls++;
      expect(input.connectionIdentity).toEqual(transport.identity);
      expect(Object.isFrozen(input.connectionIdentity)).toBe(true);
      return stream(encode([terminal()]));
    };
    const client = new ResponsesClient({ id: "provider", transport });
    expect(failure(await collect(client))).toBeUndefined();
    Object.assign(transport.identity, { generation: 2 });
    expect(failure(await collect(client))).toBe("connection_changed");
    expect(calls).toBe(1);
  });

  it("stalled reader cancellation cannot hold an aborted request", async () => {
    const { transport } = harness();
    const controller = new AbortController();
    let cancelled = false;
    transport.request = async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          start(output) {
            output.enqueue(
              encode([
                {
                  type: "response.created",
                  response: { id: "response", status: "in_progress" },
                },
              ]),
            );
          },
          cancel() {
            cancelled = true;
            return new Promise<void>(() => {});
          },
        }),
      );
    const pending = collect(
      new ResponsesClient({ id: "provider", transport }),
      {
        ...request,
        signal: controller.signal,
        onActivity() {
          controller.abort();
        },
      },
    );
    expect((await pending).at(-1)?.type).toBe("run.cancelled");
    expect(cancelled).toBe(true);
  });

  it.each([401, 403, 429, 500])(
    "HTTP %i safe category, no fallback",
    async (status) => {
      const { transport } = harness();
      let calls = 0;
      transport.request = async () => {
        calls++;
        return new Response('{"error":{"message":"secret-token"}}', { status });
      };
      const events = await collect(
        new ResponsesClient({ id: "provider", transport }),
      );
      expect(failure(events)).toBe(
        (
          {
            401: "authentication_required",
            403: "consent_required",
            429: "usage_limit",
            500: "provider_error",
          } as Record<number, string>
        )[status],
      );
      expect(calls).toBe(1);
      expect(JSON.stringify(events)).not.toContain("secret-token");
    },
  );

  it("abort wins stalled headers; late response body cancelled", async () => {
    const { transport } = harness();
    let resolve: ((value: Response) => void) | undefined;
    let started: (() => void) | undefined;
    const opened = new Promise<void>((done) => {
      started = done;
    });
    transport.request = () =>
      new Promise<Response>((done) => {
        resolve = done;
        started?.();
      });
    const controller = new AbortController();
    const pending = collect(
      new ResponsesClient({ id: "provider", transport }),
      { ...request, signal: controller.signal },
    );
    await opened;
    controller.abort();
    expect((await pending).at(-1)?.type).toBe("run.cancelled");
    let cancelled = false;
    resolve?.(
      new Response(
        new ReadableStream({
          cancel() {
            cancelled = true;
          },
        }),
      ),
    );
    await Promise.resolve();
    await Promise.resolve();
    expect(cancelled).toBe(true);
  });

  it("abort interrupts stalled stream and always cancels reader", async () => {
    const { transport } = harness();
    let cancelled = false;
    let ready: (() => void) | undefined;
    const opened = new Promise<void>((resolve) => {
      ready = resolve;
    });
    transport.request = async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(
              encode([
                {
                  type: "response.created",
                  response: { id: "response", status: "in_progress" },
                },
              ]),
            );
            ready?.();
          },
          cancel() {
            cancelled = true;
          },
        }),
      );
    const controller = new AbortController();
    const pending = collect(
      new ResponsesClient({ id: "provider", transport }),
      { ...request, signal: controller.signal },
    );
    await opened;
    controller.abort();
    expect((await pending).at(-1)?.type).toBe("run.cancelled");
    expect(cancelled).toBe(true);
  });

  it("successful EOF and early consumer return close response reader", async () => {
    for (const early of [false, true]) {
      const { transport } = harness();
      let cancelled = false;
      transport.request = async () =>
        new Response(
          new ReadableStream<Uint8Array>({
            start(controller) {
              controller.enqueue(
                encode([
                  {
                    type: "response.output_item.added",
                    output_index: 0,
                    item: message("Hi"),
                  },
                  {
                    type: "response.output_text.delta",
                    item_id: "message",
                    output_index: 0,
                    content_index: 0,
                    delta: "Hi",
                  },
                  terminal(),
                ]),
              );
              if (!early) controller.close();
            },
            cancel() {
              cancelled = true;
            },
          }),
        );
      const client = new ResponsesClient({ id: "provider", transport });
      if (!early) expect(failure(await collect(client))).toBeUndefined();
      else {
        const iterator = client.streamChat(request)[Symbol.asyncIterator]();
        await iterator.next();
        await iterator.next();
        await iterator.return?.();
        expect(cancelled).toBe(true);
      }
    }
  });
});
