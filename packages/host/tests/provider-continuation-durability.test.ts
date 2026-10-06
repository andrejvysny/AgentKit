import { describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SqliteAssistantStore } from "../../adapters-sqlite/src/index.js";
import { createTestClock } from "./fakes.js";
import {
  execute,
  fixture,
  outputText,
  response,
  submitRun,
} from "./provider-continuation-helpers.js";

describe("Responses lost durable state", () => {
  it("latest saved state loss cannot fall back to an older ancestor", async () => {
    const directory = mkdtempSync(join(tmpdir(), "agentkit-responses-loss-"));
    const path = join(directory, "state.sqlite");
    const clock = createTestClock();
    const store = new SqliteAssistantStore(path, { clock });
    try {
      const environment = await fixture({
        store,
        output: (call) => [
          response([outputText(`Answer ${call}`, `message-${call}`)]),
        ],
      });
      const first = await submitRun(environment);
      const second = await submitRun(environment, "Continue");
      expect(await store.continuations.getByRun(first.runId)).not.toBeNull();
      expect(await store.continuations.getByRun(second.runId)).not.toBeNull();
      const corruption = new Database(path);
      try {
        corruption
          .query("DELETE FROM provider_continuations WHERE run_id = ?")
          .run(second.runId);
      } finally {
        corruption.close();
      }
      const submitted = await environment.runner.submitMessage({
        chatId: environment.chatId,
        content: "Continue after loss",
      });
      await expect(
        execute(environment.runner, store, submitted.runId),
      ).rejects.toMatchObject({ code: "provider_continuation_state_missing" });
      expect(environment.sent).toHaveLength(2);
      expect((await store.tasks.getTask(submitted.runId))?.status).toBe(
        "failed",
      );
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });
});
