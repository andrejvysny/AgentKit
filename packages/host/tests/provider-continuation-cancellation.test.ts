import { describe, expect, it } from "bun:test";
import { fixture, execute } from "./provider-continuation-helpers.js";

describe("Responses host cancellation boundary", () => {
  it("cancellation after private persistence prevents tool execution", async () => {
    const environment = await fixture();
    const controller = new AbortController();
    const port = environment.store.continuations!;
    const put = port.put.bind(port);
    port.put = async (record, fence) => {
      const saved = await put(record, fence);
      controller.abort(new DOMException("Cancelled", "AbortError"));
      return saved;
    };
    const submitted = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Find",
    });
    await execute(
      environment.runner,
      environment.store,
      submitted.runId,
      controller.signal,
    );
    expect(environment.sent).toHaveLength(1);
    expect(environment.executed()).toBe(0);
    expect(
      (await environment.store.tasks.getTask(submitted.runId))?.status,
    ).toBe("cancelled");
    expect(await port.getByRun(submitted.runId)).not.toBeNull();
    const messages = await environment.store.conversations.listMessages(
      environment.chatId,
    );
    expect(
      messages.filter((message) => message.metadata.canonicalProviderTurn),
    ).toHaveLength(0);
    expect(JSON.stringify(messages)).not.toContain("private-encrypted-state");
  });
});
