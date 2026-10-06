import { describe, expect, it } from "bun:test";
import { collect, harness, request, terminal } from "./responses-helpers.js";

function usageEvents(events: Awaited<ReturnType<typeof collect>>) {
  return events.filter((event) => event.type === "run.usage");
}

describe("Responses failure accounting and valid activity", () => {
  it.each(["response.failed", "response.incomplete"])(
    "retains billed usage on %s exactly once",
    async (type) => {
      const events = await collect(
        harness([
          {
            type,
            response: {
              error: { code: "subscription_sharing_usage_limit_exceeded" },
              usage: { input_tokens: 20, output_tokens: 3, total_tokens: 23 },
            },
          },
        ]).client,
      );
      expect(usageEvents(events)).toHaveLength(1);
      expect(usageEvents(events)[0]?.data).toMatchObject({
        finalForCall: false,
        promptTokens: 20,
        completionTokens: 3,
        totalTokens: 23,
      });
      expect(events.at(-1)?.type).toBe("run.failed");
      expect(events.some((event) => event.type === "run.tool.requested")).toBe(
        false,
      );
    },
  );

  it("completed followed by failure retains usage without successful completion", async () => {
    const events = await collect(
      harness([
        terminal(),
        { type: "response.failed", response: { error: { code: "bad" } } },
      ]).client,
    );
    expect(usageEvents(events)).toHaveLength(1);
    expect(usageEvents(events)[0]?.data.finalForCall).toBe(false);
    expect(events.at(-1)?.type).toBe("run.failed");
  });

  it("unknown/malformed stream events never count as activity", async () => {
    for (const event of [
      { type: "new.event" },
      { type: "response.created", response: {} },
      {
        type: "response.reasoning_summary_text.delta",
        item_id: "missing",
        output_index: 0,
        delta: "hidden",
      },
    ]) {
      let activity = 0;
      const events = await collect(harness([event, terminal()]).client, {
        ...request,
        onActivity() {
          activity++;
        },
      });
      expect(activity).toBe(0);
      expect(events.at(-1)?.type).toBe("run.failed");
    }
  });
});
