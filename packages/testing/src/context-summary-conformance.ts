// Shared behavioral contract for the optional AssistantStore.summaries port
// (ADR 0017 part 2). Framework-neutral like the other suites.
import type { ContextSummaryRecord, ContextSummaryStore } from "@agentkit/host";
import type { DescribeAssistantStoreConformanceOptions } from "./conformance-support.js";

export type DescribeContextSummaryStoreConformanceOptions = Pick<
  DescribeAssistantStoreConformanceOptions,
  "create" | "test"
> & { name: string };

export function describeContextSummaryStoreConformance(
  options: DescribeContextSummaryStoreConformanceOptions,
): void {
  const { name, create, test } = options;
  const { describe, it, expect } = test;

  let counter = 0;
  const summary = (
    overrides: Partial<ContextSummaryRecord> = {},
  ): ContextSummaryRecord => {
    counter += 1;
    return {
      summaryId: `summary-${counter}`,
      sessionId: "chat-1",
      coversThroughMessageId: "m-2",
      firstKeptMessageId: "m-3",
      summary: "## Goal\nship it",
      sourceHash: "a".repeat(64),
      tokensBefore: 1000,
      tokensAfter: 200,
      model: "model-a",
      summarizerRevision: "agentkit-compaction-v1",
      details: { readFiles: ["a.ts"], modifiedFiles: ["b.ts"] },
      createdAt: `2030-01-01T00:00:0${counter % 10}.000Z`,
      ...overrides,
    };
  };

  const withStore = async (
    fn: (summaries: ContextSummaryStore) => Promise<void>,
  ): Promise<void> => {
    const harness = await create();
    try {
      const summaries = harness.store.summaries;
      expect(summaries).toBeDefined();
      if (!summaries) return;
      await fn(summaries);
    } finally {
      harness.close?.();
    }
  };

  describe(`ContextSummaryStore conformance — ${name}`, () => {
    it("saves and lists a record, with and without providerId", async () => {
      await withStore(async (summaries) => {
        const withProvider = summary({ providerId: "prov-1" });
        const without = summary({
          sessionId: "chat-2",
          createdAt: "2030-02-01T00:00:00.000Z",
        });
        await summaries.saveSummary(withProvider);
        await summaries.saveSummary(without);
        expect(await summaries.listSummaries("chat-1")).toEqual([withProvider]);
        const listed = await summaries.listSummaries("chat-2");
        expect(listed).toEqual([without]);
        expect(listed[0]?.providerId).toBeUndefined();
        expect(await summaries.listSummaries("missing")).toEqual([]);
      });
    });

    it("lists newest first", async () => {
      await withStore(async (summaries) => {
        const old = summary({ createdAt: "2030-01-01T00:00:00.000Z" });
        const newer = summary({ createdAt: "2030-01-03T00:00:00.000Z" });
        const middle = summary({ createdAt: "2030-01-02T00:00:00.000Z" });
        for (const record of [old, newer, middle])
          await summaries.saveSummary(record);
        const ids = (await summaries.listSummaries("chat-1")).map(
          (record) => record.summaryId,
        );
        expect(ids).toEqual([newer.summaryId, middle.summaryId, old.summaryId]);
      });
    });

    it("saving the same summaryId again is idempotent", async () => {
      await withStore(async (summaries) => {
        const record = summary();
        await summaries.saveSummary(record);
        await summaries.saveSummary({ ...record, summary: "changed" });
        const listed = await summaries.listSummaries("chat-1");
        expect(listed.length).toBe(1);
        expect(listed[0]?.summary).toBe(record.summary);
      });
    });

    it("deletes only the named session and counts the rows", async () => {
      await withStore(async (summaries) => {
        await summaries.saveSummary(summary());
        await summaries.saveSummary(summary());
        await summaries.saveSummary(summary({ sessionId: "chat-2" }));
        expect(await summaries.deleteSessionSummaries("chat-1")).toBe(2);
        expect(await summaries.listSummaries("chat-1")).toEqual([]);
        expect((await summaries.listSummaries("chat-2")).length).toBe(1);
        expect(await summaries.deleteSessionSummaries("chat-1")).toBe(0);
      });
    });
  });
}
