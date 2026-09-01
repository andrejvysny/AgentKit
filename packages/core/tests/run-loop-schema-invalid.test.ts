import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { messageContentToText } from "../src/messages/content.js";
import { MockProviderClient } from "@agentkit/testing";
import { collectRun } from "./helpers.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";

/**
 * A tool whose schema requires a string `text` and forbids extra keys. Records
 * whether it was executed so we can assert validation runs BEFORE execution.
 */
function makeStrictTool(): {
  tool: AiTool<{ text: string }, { echoed: string }>;
  executed: () => boolean;
} {
  let ran = false;
  const tool: AiTool<{ text: string }, { echoed: string }> = {
    definition: {
      name: "strict",
      version: "1",
      effect: "read",
      capability: "test",
      description: "requires text",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
        additionalProperties: false,
      },
    },
    async execute(ctx, input) {
      ran = true;
      return {
        ok: true,
        data: { echoed: input.text },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
  return { tool, executed: () => ran };
}

describe("runChat — schema_invalid arguments", () => {
  it("fails with errorCode schema_invalid and does not execute the tool", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "strict",
            // `text` is the wrong type (number) — Ajv rejects it.
            argumentsJson: '{"text":42}',
          },
        ],
      },
      { steps: [{ kind: "text", content: "ack" }] },
    ]);
    const { tool, executed } = makeStrictTool();
    const registry = new AiToolRegistry();
    registry.register(tool as unknown as AiTool);
    const messages: AiChatMessage[] = [{ role: "user", content: "go" }];
    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages,
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    // No execution, no success — validation gates before the tool runs.
    expect(executed()).toBe(false);
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(false);

    const failed = events.find((e) => e.type === "run.tool.failed") as
      | (AiRunEvent & { data: { errorCode?: string; errorMessage: string } })
      | undefined;
    expect(failed).toBeDefined();
    expect(failed!.data.errorCode).toBe("schema_invalid");

    // The model is fed the balanced error envelope (F10), not a raw {ok,error}.
    const toolMsg = result.appendedMessages.find((m) => m.role === "tool");
    expect(toolMsg).toBeDefined();
    const env = JSON.parse(messageContentToText(toolMsg!.content)) as {
      ok: boolean;
      status: string;
      warnings: string[];
      truncated: boolean;
      data: {
        errorCode: string;
        errorMessage: string;
        phase?: string;
        retryable?: boolean;
      };
    };
    expect(env.ok).toBe(false);
    expect(env.status).toBe("error");
    expect(env.warnings).toEqual([]);
    expect(env.truncated).toBe(false);
    expect(env.data.errorCode).toBe("schema_invalid");
    expect(env.data.errorMessage).toContain("strict");
    // The stage is named so the model can tell "you wrote the wrong arguments"
    // from "it broke" — and `retryable: false` because THESE arguments will
    // fail the same way every time.
    expect(env.data.phase).toBe("validation");
    expect(env.data.retryable).toBe(false);
  });
});
