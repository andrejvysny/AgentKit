import { afterEach, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { STANDING_APPROVALS_METADATA_KEY } from "../src/index.js";
import {
  open,
  drive,
  start,
  pending,
  resume,
  type State,
} from "./fixtures/approval-fixture.js";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});

function dbPath(): string {
  const dir = mkdtempSync(join(tmpdir(), "approval-recovery-"));
  dirs.push(dir);
  return join(dir, "store.db");
}

for (const legacyState of [
  "pending",
  "answered-unused",
  "uncertain",
] as const) {
  it.skipIf(process.env.AGENTKIT_LEGACY_PACKAGE_ROOT === undefined)(
    `actual .14 upgrade: preserves ${legacyState}, grants, history and settings`,
    async () => {
      const path = dbPath();
      const state: State = { effects: 0 };
      let f = await open(
        path,
        state,
        process.env.AGENTKIT_LEGACY_PACKAGE_ROOT!,
      );
      const runId = await start(f);
      const request = await pending(f);
      expect(request.content).not.toHaveProperty("contextSha256");
      expect(f.store.toolExecutions).toBeUndefined();
      const grant = {
        grantId: "legacy-grant",
        digest: "a".repeat(64),
        label: "legacy command",
        toolName: "write",
        sourceRequestId: request.inputId,
        answerId: "grant-answer",
        grantedAt: new Date().toISOString(),
      };
      const chat = (await f.store.conversations.getChat("chat"))!;
      await f.store.conversations.updateChat("chat", {
        metadata: {
          ...chat.metadata,
          [STANDING_APPROVALS_METADATA_KEY]: [grant],
        },
      });
      await f.store.settings.updateSettings({ defaultModel: "legacy-setting" });
      if (legacyState !== "pending")
        await f.service.approve(request.inputId, "legacy-answer");
      if (legacyState === "uncertain") {
        state.crash = "after_effect";
        await expect(drive(f)).rejects.toThrow();
        expect(state.effects).toBe(1);
      }
      f.store.close();
      f = await open(path, state);
      expect(
        (await f.store.conversations.getChat("chat"))?.metadata[
          STANDING_APPROVALS_METADATA_KEY
        ],
      ).toEqual([grant]);
      expect(
        (await f.store.conversations.listMessages("chat")).some(
          (record) => record.content === "write once",
        ),
      ).toBe(true);
      expect((await f.store.settings.getSettings()).defaultModel).toBe(
        "legacy-setting",
      );
      if (legacyState === "pending") {
        expect((await pending(f)).inputId).toBe(request.inputId);
        expect(state.effects).toBe(0);
      } else if (legacyState === "answered-unused") {
        await drive(f);
        expect(state.effects).toBe(1);
        expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
      } else {
        await resume(f, runId);
        const fresh = await pending(f);
        expect(fresh.inputId).not.toBe(request.inputId);
        expect(state.effects).toBe(1);
        f.store.close();
        f = await open(path, state);
        await f.service.approve(fresh.inputId, "fresh-answer");
        await Promise.all([drive(f), drive(f)]);
        expect(state.effects).toBe(2);
      }
      f.store.close();
    },
  );
}
