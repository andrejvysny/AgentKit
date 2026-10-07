import { describe, expect, it } from "bun:test";
import type { AiRunEvent, AiToolDefinition } from "@agentkit/contracts";
import {
  execute,
  outputText,
  reasoning,
  response,
  submitRun,
} from "./provider-continuation-helpers.js";
import {
  callId,
  catalog,
  catalogDigest,
  consumerFixture,
  nativeCall,
} from "./responses-openpcb-helpers.js";

function expectCatalogRequests(
  environment: Awaited<ReturnType<typeof consumerFixture>>,
): void {
  for (const request of environment.sent) {
    expect(request.body?.tools).toEqual([
      {
        type: "namespace",
        name: "agentkit",
        description: "Tools supplied by the application.",
        tools: catalog.tools.map((tool) => ({
          type: "function",
          name: tool.name,
          description: tool.description,
          parameters: tool.inputSchema,
          strict: false,
        })),
      },
    ]);
  }
}

function expectNativeReplay(
  environment: Awaited<ReturnType<typeof consumerFixture>>,
): void {
  const input = environment.sent[1]?.body?.input as Record<string, unknown>[];
  expect(input.filter((item) => item.type === "function_call")).toEqual([
    nativeCall,
  ]);
  const outputs = input.filter((item) => item.type === "function_call_output");
  expect(outputs).toHaveLength(1);
  expect(outputs[0]?.call_id).toBe(callId);
  expect(String(outputs[0]?.output)).toContain('"parts":17');
  expect(String(outputs[0]?.output)).not.toContain("host-only");
}

describe("pinned OpenPCB Responses consumer catalog through TurnRunner", () => {
  it("pins the published source and verifies all native definitions", () => {
    expect(catalog.provenance.commit).toBe(
      "d69189fa88605e140af01c1246e2da54b04f2201",
    );
    expect(catalog.provenance.sourceRawSha256).toBe(
      "77b48a68698c65ffc8a1a88a90fec11f7ba9192882b613ac8bee3dea39a2d4d6",
    );
    expect(catalog.provenance.sourceCanonicalDataSha256).toBe(
      "a64bdf2045794bd54b250887d6b6af2c4d42e16355e9681b0a41723325aefb01",
    );
    expect(catalog.tools).toHaveLength(15);
    expect(catalogDigest()).toBe(
      "22aa56a046f7c3000a3f894eaddb7959297a46e9252950ff1efac87a06e75eaa",
    );
    expect(catalogDigest()).toBe(catalog.provenance.toolsSha256);
  });

  it.each(["api-key", "chatgpt-account"] as const)(
    "%s preserves every schema and exact native call/result through continuation",
    async (profile) => {
      const environment = await consumerFixture(profile, (call) => [
        response(
          call === 1
            ? [reasoning, outputText("Reading design"), nativeCall]
            : [outputText("17 parts")],
        ),
      ]);
      const submitted = await submitRun(environment);
      expect(
        (await environment.store.tasks.getTask(submitted.runId))?.status,
      ).toBe("completed");
      expect(environment.nativeExecuted).toEqual([
        { designId: "fixture-design" },
      ]);
      expect(environment.sent).toHaveLength(2);
      expectCatalogRequests(environment);
      expectNativeReplay(environment);
      const events = (await environment.store.tasks.listEvents(
        submitted.runId,
      )) as AiRunEvent[];
      expect(events.filter((event) => event.type === "run.usage")).toHaveLength(
        2,
      );
      expect(
        environment.recordedUsage.map((usage) => usage.totalTokens),
      ).toEqual([5, 5]);
      expect(
        new Set(environment.recordedUsage.map((usage) => usage.callId)).size,
      ).toBe(2);
      expect(events.at(-1)?.type).toBe("run.completed");
      const publicData = JSON.stringify([
        events,
        await environment.store.conversations.listMessages(environment.chatId),
      ]);
      expect(publicData).not.toContain("private-encrypted-state");
      expect(publicData).not.toContain("inputItems");
    },
  );

  it("cancellation after private persistence starts no native tool", async () => {
    const environment = await consumerFixture("chatgpt-account", () => [
      response([reasoning, nativeCall]),
    ]);
    const controller = new AbortController();
    const continuation = environment.store.continuations!;
    const put = continuation.put.bind(continuation);
    continuation.put = async (record, fence) => {
      const saved = await put(record, fence);
      controller.abort(new DOMException("Cancelled", "AbortError"));
      return saved;
    };
    const submitted = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Read design",
    });
    await execute(
      environment.runner,
      environment.store,
      submitted.runId,
      controller.signal,
    );
    expect(
      (await environment.store.tasks.getTask(submitted.runId))?.status,
    ).toBe("cancelled");
    expect(environment.nativeExecuted).toHaveLength(0);
    expect(environment.sent).toHaveLength(1);
    const events = await environment.store.tasks.listEvents(submitted.runId);
    expect(events.some((event) => event.type === "run.tool.running")).toBe(
      false,
    );
    expect(events.some((event) => event.type === "run.completed")).toBe(false);
  });

  it("unsupported native schema fails before transport and execution", async () => {
    const unsupported = catalog.tools.map((tool, index) =>
      index === 0
        ? {
            ...tool,
            inputSchema: {
              ...tool.inputSchema,
              pattern: "unsupported-by-responses",
            },
          }
        : tool,
    ) as AiToolDefinition[];
    const environment = await consumerFixture(
      "chatgpt-account",
      () => [],
      unsupported,
    );
    const submitted = await submitRun(environment);
    expect(
      (await environment.store.tasks.getTask(submitted.runId))?.status,
    ).toBe("failed");
    expect(environment.sent).toHaveLength(0);
    expect(environment.nativeExecuted).toHaveLength(0);
    const events = (await environment.store.tasks.listEvents(
      submitted.runId,
    )) as AiRunEvent[];
    expect(
      events.find((event) => event.type === "run.failed")?.data,
    ).toMatchObject({ errorCode: "unsupported_tool_schema" });
  });

  it("text then terminal usage then late failure never dispatches or completes", async () => {
    const environment = await consumerFixture("chatgpt-account", () => [
      {
        type: "response.output_item.added",
        output_index: 0,
        item: { ...outputText("", "answer"), status: "in_progress" },
      },
      {
        type: "response.output_text.delta",
        item_id: "answer",
        output_index: 0,
        content_index: 0,
        delta: "Partial design answer",
      },
      response([outputText("Partial design answer", "answer"), nativeCall]),
      {
        type: "response.failed",
        response: { error: { code: "server_error" } },
      },
    ]);
    const submitted = await submitRun(environment);
    expect(
      (await environment.store.tasks.getTask(submitted.runId))?.status,
    ).toBe("failed");
    expect(environment.sent).toHaveLength(1);
    expect(environment.nativeExecuted).toHaveLength(0);
    const events = (await environment.store.tasks.listEvents(
      submitted.runId,
    )) as AiRunEvent[];
    expect(events.some((event) => event.type === "run.message.delta")).toBe(
      true,
    );
    expect(events.some((event) => event.type === "run.completed")).toBe(false);
    expect(events.some((event) => event.type === "run.tool.running")).toBe(
      false,
    );
    const usage = events.filter((event) => event.type === "run.usage");
    expect(usage).toHaveLength(1);
    expect(usage[0]?.data).toMatchObject({
      promptTokens: 2,
      completionTokens: 3,
      totalTokens: 5,
      finalForCall: false,
    });
    expect(environment.recordedUsage).toHaveLength(1);
    expect(environment.recordedUsage[0]).toMatchObject({
      totalTokens: 5,
      finalForCall: false,
    });
  });
});
