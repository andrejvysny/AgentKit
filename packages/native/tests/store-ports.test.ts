import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeStore } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;
const provider = {
  id: "p",
  label: "Provider",
  kind: "openai-compatible",
  baseUrl: "http://localhost",
  defaultModel: "m",
  enabled: true,
};
const summary = {
  summaryId: "sum",
  sessionId: "s",
  coversThroughMessageId: "a",
  firstKeptMessageId: "b",
  summary: "first",
  sourceHash: "0".repeat(64),
  tokensBefore: 100,
  tokensAfter: 10,
  model: "m",
  summarizerRevision: "agentkit-compaction-v1",
  details: { readFiles: ["one.rs"], modifiedFiles: [] },
  createdAt: "2026-10-30T00:00:00.000Z",
};

nativeTest(
  "provider ports preserve catalogues and clear related records on delete",
  async () => {
    const store = await NativeStore.inMemory();
    try {
      expect(
        await store.callStore<typeof provider>("providers.upsert", provider),
      ).toEqual(provider);
      await store.callStore("providers.replaceModels", {
        providerId: "p",
        models: [
          {
            providerId: "p",
            modelId: "z",
            displayName: null,
            fetchedAt: "now",
          },
          { providerId: "p", modelId: "a", displayName: "A", fetchedAt: "now" },
        ],
      });
      const models = await store.callStore<{ modelId: string }[]>(
        "providers.listModels",
        { providerId: "p" },
      );
      expect(models.map((model) => model.modelId)).toEqual(["a", "z"]);
      await store.callStore("providers.saveCapabilities", {
        providerId: "p",
        capabilities: { streaming: true, toolCalling: true, modelList: true },
      });
      expect(
        await store.callStore<{ streaming: boolean }>(
          "providers.getCapabilities",
          { providerId: "p" },
        ),
      ).toMatchObject({ streaming: true });
      await store.callStore("providers.delete", { providerId: "p" });
      expect(
        await store.callStore<(typeof provider)[]>("providers.list"),
      ).toEqual([]);
      expect(
        await store.callStore<{ modelId: string }[]>("providers.listModels", {
          providerId: "p",
        }),
      ).toEqual([]);
      expect(
        await store.callStore("providers.getCapabilities", { providerId: "p" }),
      ).toBeNull();
      await expect(
        store.callStore("providers.execute", {}),
      ).rejects.toMatchObject({ code: "not_implemented" });
    } finally {
      await store.close();
    }
    await expect(store.callStore("providers.list")).rejects.toMatchObject({
      code: "closed",
    });
  },
);

nativeTest(
  "native provider and summary ports reopen with first-write summary identity",
  async () => {
    const directory = mkdtempSync(join(tmpdir(), "native-store-ports-"));
    let store = await NativeStore.open(join(directory, "store.sqlite"));
    try {
      await store.callStore("providers.upsert", provider);
      await store.callStore("summaries.save", summary);
      await store.callStore("summaries.save", {
        ...summary,
        sessionId: "other",
        summary: "replacement",
      });
      await store.close();
      store = await NativeStore.open(join(directory, "store.sqlite"));
      expect(
        await store.callStore<typeof provider>("providers.get", {
          providerId: "p",
        }),
      ).toEqual(provider);
      expect(
        await store.callStore<(typeof summary)[]>("summaries.list", {
          sessionId: "s",
        }),
      ).toEqual([summary]);
      expect(
        await store.callStore<(typeof summary)[]>("summaries.list", {
          sessionId: "other",
        }),
      ).toEqual([]);
      expect(
        await store.callStore<number>("summaries.deleteSession", {
          sessionId: "s",
        }),
      ).toBe(1);
      expect(
        await store.callStore<(typeof summary)[]>("summaries.list", {
          sessionId: "s",
        }),
      ).toEqual([]);
    } finally {
      await store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  },
);
