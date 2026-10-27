/**
 * Session context compactor (ADR 0017 part 2): cut selection, serialization,
 * file details, the update prompt and the failure results.
 */
import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import { defaultTokenEstimator, type AiChatRequest } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  COMPACTION_SUMMARIZER_REVISION,
  SUMMARIZATION_PROMPT,
  SUMMARIZATION_SYSTEM_PROMPT,
  UPDATE_SUMMARIZATION_PROMPT,
  applyValidSummary,
  compactSessionContext,
  findCut,
  hashSource,
  serializeConversation,
  type CompactSessionContextInput,
  type ContextSummaryRecord,
} from "../src/index.js";

class SpyProvider extends MockProviderClient {
  readonly requests: AiChatRequest[] = [];
  throws = false;
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(structuredClone(input));
    if (this.throws) throw new Error("provider down with secret text");
    return super.streamChat(input);
  }
}

const call = (id: string, name: string, path: string) => ({
  id,
  name,
  argumentsJson: JSON.stringify({ path }),
});

/** Three turns: read a.ts, edit b.ts, then the current question. */
function transcript(): { messages: AiChatMessage[]; recordIds: string[] } {
  const messages: AiChatMessage[] = [
    { role: "system", content: "sys" },
    { role: "user", content: "q1" },
    {
      role: "assistant",
      content: "",
      toolCalls: [call("c1", "read_file", "a.ts")],
    },
    { role: "tool", content: "r".repeat(5000), toolCallId: "c1" },
    { role: "assistant", content: "done1" },
    { role: "user", content: "q2" },
    {
      role: "assistant",
      content: "",
      toolCalls: [call("c2", "edit_file", "b.ts")],
    },
    { role: "tool", content: "edited", toolCallId: "c2" },
    { role: "assistant", content: "done2" },
    { role: "user", content: "q3" },
  ];
  return { messages, recordIds: messages.slice(1).map((_, i) => `m${i}`) };
}

function baseInput(
  provider: SpyProvider,
  overrides: Partial<CompactSessionContextInput> = {},
): CompactSessionContextInput {
  const { messages, recordIds } = transcript();
  return {
    sessionId: "s1",
    messages,
    recordIds,
    client: provider,
    model: "m1",
    estimator: defaultTokenEstimator,
    windowTokens: 8000,
    reserveTokens: 1000,
    keepRecentTokens: 5,
    limits: {
      maxItems: 10,
      maxTextChars: 1000,
      maxBytes: 1000,
      timeoutMs: 1000,
    } as never,
    taskId: "task-1",
    compactionNumber: 1,
    summaryId: "task-1:compaction:1",
    nowIso: "2026-01-01T00:00:00.000Z",
    ...overrides,
  };
}

function summarizer(text = "## Goal\nfixed"): SpyProvider {
  const provider = new SpyProvider();
  provider.setScript([{ steps: [{ kind: "text", content: text }] }]);
  return provider;
}

describe("findCut", () => {
  it("cuts only at turn starts, so a call and its results stay together", () => {
    const { messages } = transcript();
    const body = messages.slice(1);
    for (const keep of [0, 1, 5, 20, 60, 400, 5000, 100_000]) {
      const cut = findCut(body, 0, keep, defaultTokenEstimator);
      expect(body[cut]?.role).toBe("user");
      expect(body[cut - 1]?.role).not.toBe("tool");
    }
  });

  it("always keeps the current turn, however large", () => {
    const body: AiChatMessage[] = [
      { role: "user", content: "old" },
      { role: "assistant", content: "a" },
      { role: "user", content: "x".repeat(4000) },
    ];
    expect(findCut(body, 0, 1, defaultTokenEstimator)).toBe(2);
  });

  it("keeps older whole turns while they fit the keep budget", () => {
    const { messages } = transcript();
    const body = messages.slice(1);
    expect(findCut(body, 0, 100_000, defaultTokenEstimator)).toBe(0);
    // Turn 2 (q2 .. done2) fits, turn 1 (5000-char result) does not.
    expect(findCut(body, 0, 100, defaultTokenEstimator)).toBe(4);
  });
});

describe("compactSessionContext", () => {
  it("summarizes the older turns and keeps the current one", async () => {
    const provider = summarizer();
    const outcome = await compactSessionContext(baseInput(provider));
    expect(outcome?.ok).toBe(true);
    if (!outcome?.ok) return;
    expect(outcome.record.coversThroughMessageId).toBe("m7");
    expect(outcome.record.firstKeptMessageId).toBe("m8");
    expect(outcome.coveredMessages).toBe(8);
    expect(outcome.record.summarizerRevision).toBe(
      COMPACTION_SUMMARIZER_REVISION,
    );
    expect(outcome.record.sourceHash).toMatch(/^[0-9a-f]{64}$/);
    expect(outcome.record.summary.startsWith("## Goal\nfixed")).toBe(true);
    const request = provider.requests[0];
    expect(request?.runId).toBe("task-1:compaction:1");
    expect(request?.tools ?? []).toEqual([]);
    expect(request?.messages[0]).toEqual({
      role: "system",
      content: SUMMARIZATION_SYSTEM_PROMPT,
    });
    const user = String(request?.messages[1]?.content);
    expect(user).toContain("[User]: q1");
    expect(user).toContain("[Assistant tool calls]: read_file(path=");
    expect(user).not.toContain("q3");
    expect(user.endsWith(SUMMARIZATION_PROMPT)).toBe(true);
  });

  it("returns undefined when only the current turn exists", async () => {
    const provider = summarizer();
    const input = baseInput(provider);
    const outcome = await compactSessionContext({
      ...input,
      messages: input.messages.slice(0, 1).concat(input.messages.slice(-1)),
      recordIds: input.recordIds.slice(-1),
    });
    expect(outcome).toBeUndefined();
    expect(provider.requests).toHaveLength(0);
  });

  it("extracts read and modified files into details and the summary", async () => {
    const outcome = await compactSessionContext(baseInput(summarizer()));
    if (!outcome?.ok) throw new Error("expected a summary");
    expect(outcome.record.details).toEqual({
      readFiles: ["a.ts"],
      modifiedFiles: ["b.ts"],
    });
    expect(outcome.record.summary).toContain(
      "<read-files>\na.ts\n</read-files>",
    );
    expect(outcome.record.summary).toContain(
      "<modified-files>\nb.ts\n</modified-files>",
    );
  });

  it("uses the update prompt and the previous summary, covering only newer turns", async () => {
    const provider = summarizer("## Goal\nupdated");
    const input = baseInput(provider);
    const previous: ContextSummaryRecord = {
      summaryId: "old",
      sessionId: "s1",
      coversThroughMessageId: "m3",
      firstKeptMessageId: "m4",
      summary: "PREVIOUS SUMMARY BODY",
      sourceHash: "x",
      tokensBefore: 1,
      tokensAfter: 1,
      model: "m1",
      summarizerRevision: COMPACTION_SUMMARIZER_REVISION,
      details: { readFiles: ["old.ts"], modifiedFiles: [] },
      createdAt: "2026-01-01T00:00:00.000Z",
    };
    const outcome = await compactSessionContext({ ...input, previous });
    if (!outcome?.ok) throw new Error("expected a summary");
    const user = String(provider.requests[0]?.messages[1]?.content);
    expect(user).toContain(
      "<previous-summary>\nPREVIOUS SUMMARY BODY\n</previous-summary>",
    );
    expect(user.endsWith(UPDATE_SUMMARIZATION_PROMPT)).toBe(true);
    expect(user).not.toContain("[User]: q1");
    expect(user).toContain("[User]: q2");
    expect(outcome.record.details.readFiles).toEqual(["old.ts"]);
    expect(outcome.record.details.modifiedFiles).toEqual(["b.ts"]);
  });

  it("fails soft: provider error, empty text, and no leak of the thrown text", async () => {
    const down = summarizer();
    down.throws = true;
    const failed = await compactSessionContext(baseInput(down));
    expect(failed?.ok).toBe(false);
    if (failed && !failed.ok) expect(failed.reason).not.toContain("secret");

    const empty = summarizer("   ");
    const outcome = await compactSessionContext(baseInput(empty));
    expect(outcome?.ok).toBe(false);
  });
});

describe("serializeConversation", () => {
  it("truncates tool results at 2000 characters", () => {
    const text = serializeConversation([
      { role: "tool", content: "z".repeat(2500), toolCallId: "c" },
    ]);
    expect(text).toContain("z".repeat(2000));
    expect(text).not.toContain("z".repeat(2001));
    expect(text).toContain("[... 500 more characters truncated]");
  });

  it("labels each role", () => {
    const text = serializeConversation([
      { role: "user", content: "hi" },
      {
        role: "assistant",
        content: "ok",
        toolCalls: [{ id: "c", name: "t", argumentsJson: '{"a":1}' }],
      },
    ]);
    expect(text).toBe(
      "[User]: hi\n\n[Assistant]: ok\n\n[Assistant tool calls]: t(a=1)",
    );
  });
});

describe("applyValidSummary", () => {
  async function record(): Promise<ContextSummaryRecord> {
    const { messages, recordIds } = transcript();
    const body = messages.slice(1);
    return {
      summaryId: "s",
      sessionId: "s1",
      coversThroughMessageId: "m3",
      firstKeptMessageId: "m4",
      summary: "SUM",
      sourceHash: await hashSource(body.slice(0, 4), recordIds.slice(0, 4)),
      tokensBefore: 1,
      tokensAfter: 1,
      model: "m1",
      summarizerRevision: COMPACTION_SUMMARIZER_REVISION,
      details: { readFiles: [], modifiedFiles: [] },
      createdAt: "2026-01-01T00:00:00.000Z",
    };
  }

  it("replaces the covered prefix with one user summary message", async () => {
    const { messages, recordIds } = transcript();
    const applied = await applyValidSummary(
      messages,
      recordIds,
      await record(),
    );
    expect(applied?.messages[0]).toEqual({ role: "system", content: "sys" });
    expect(applied?.messages[1]).toEqual({
      role: "user",
      content:
        "The conversation history before this point was compacted into the following summary:\n<summary>\nSUM\n</summary>",
    });
    expect(applied?.messages).toHaveLength(7);
    expect(applied?.recordIds[1]).toBe("m4");
  });

  it("is ignored when a covered record was edited, moved or the revision differs", async () => {
    const { messages, recordIds } = transcript();
    const valid = await record();
    const edited = messages.map((m, i) =>
      i === 1 ? { ...m, content: "q1 edited" } : m,
    );
    expect(await applyValidSummary(edited, recordIds, valid)).toBeUndefined();
    const renamed = recordIds.map((id) => (id === "m3" ? "other" : id));
    expect(await applyValidSummary(messages, renamed, valid)).toBeUndefined();
    expect(
      await applyValidSummary(messages, recordIds, {
        ...valid,
        summarizerRevision: "agentkit-compaction-v0",
      }),
    ).toBeUndefined();
  });
});
