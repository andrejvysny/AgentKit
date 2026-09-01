// `searchMessages`: the OPTIONAL half of the conversation contract, graded
// behind `capabilities.search`.
//
// The tests below are written to the PROMISES the port makes, not to either
// adapter's mechanism — one is a substring scan and the other is FTS5 with
// bm25, so they will never return the same snippet for the same message. What
// they must agree on is which messages are hits, which are excluded, that the
// markers are the shared ones, and that a query full of punctuation answers
// with results rather than an exception.
//
// FRAMEWORK-NEUTRAL, same rules as the rest of this package: no runner import,
// every `@agentkit/host` import is `import type`, and error assertions match on
// the `code` string rather than on `instanceof`.
import type { AiContentPart } from "@agentkit/contracts";
import type { AssistantStore, MessageSearchHit } from "@agentkit/host";
import type {
  AssistantStoreConformanceHarness,
  AssistantStoreConformanceTestApi,
} from "./conformance-support.js";

export interface MessageSearchOptions {
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/**
 * The method, proved present.
 *
 * `searchMessages` is optional on the port, so a harness that declares
 * `capabilities.search` (or leaves it undefined, which means "capable") and
 * ships no method would otherwise fail with a TypeError deep inside a test
 * about ranking. This turns that into the honest failure: the capability was
 * claimed and the method is not there.
 */
function searchOf(
  store: AssistantStore,
  expect: AssistantStoreConformanceTestApi["expect"],
): (
  query: string,
  opts?: { chatId?: string; limit?: number },
) => Promise<MessageSearchHit[]> {
  const search = store.conversations.searchMessages;
  expect(typeof search).toBe("function");
  return (query, opts) =>
    (
      search as NonNullable<AssistantStore["conversations"]["searchMessages"]>
    ).call(store.conversations, query, opts);
}

/** Ids of the hits, in the order the store ranked them. */
function hitIds(hits: readonly MessageSearchHit[]): string[] {
  return hits.map((hit) => hit.messageId);
}

/**
 * A multimodal body whose searchable text is in the SECOND text part, with an
 * image between the two.
 *
 * This is the bug the shared `searchTextOf` projection exists to prevent: the
 * system this design is copied from indexed only the first text part, which
 * fails silently — the message is in the store, the term is in the message, and
 * the search box simply never finds it.
 */
function multipartBody(): AiContentPart[] {
  return [
    { type: "text", text: "here is the diagram you asked for" },
    {
      type: "image",
      source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
    },
    { type: "text", text: "the connector is a zebrafish footprint" },
  ];
}

export function describeMessageSearch(options: MessageSearchOptions): void {
  const { create, test } = options;
  const { describe, it, expect } = test;

  describe("message search", () => {
    it("finds a multimodal message by a term in its SECOND text part", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const chat = await store.conversations.createChat({});
        const message = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: multipartBody(),
        });
        // A term from part ONE still works, obviously — it is the second that
        // is the regression.
        expect(hitIds(await search("diagram"))).toEqual([message.id]);

        const hits = await search("zebrafish");
        expect(hitIds(hits)).toEqual([message.id]);
        expect(hits[0]?.chatId).toBe(chat.id);
        // The shared markers, around the term that matched.
        expect(hits[0]?.snippet).toContain("[zebrafish]");
      } finally {
        close?.();
      }
    });

    it("ranks the message that matches the whole query above ones matching half of it", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const chat = await store.conversations.createChat({});
        await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "the quartz sample is unremarkable and dull",
        });
        await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "a beacon of hope for the weary traveller",
        });
        const both = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "quartz beacon assembly notes",
        });

        const hits = await search("quartz beacon");
        expect(hits.length).toBeGreaterThan(0);
        // Best first: whatever else a store returns, the message that carries
        // the whole query is the one at the top.
        expect(hits[0]?.messageId).toBe(both.id);
        expect(hits[0]?.snippet).toContain("[");
        expect(hits[0]?.snippet).toContain("]");
        expect(hits[0]?.snippet.toLowerCase()).toContain("quartz");
      } finally {
        close?.();
      }
    });

    it("restricts to one chat when asked, and searches every chat when not", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const one = await store.conversations.createChat({});
        const two = await store.conversations.createChat({});
        const inOne = await store.conversations.appendMessage({
          chatId: one.id,
          role: "user",
          content: "the tantalum capacitor again",
        });
        const inTwo = await store.conversations.appendMessage({
          chatId: two.id,
          role: "user",
          content: "tantalum, but over here",
        });

        expect(hitIds(await search("tantalum", { chatId: one.id }))).toEqual([
          inOne.id,
        ]);
        expect(hitIds(await search("tantalum", { chatId: two.id }))).toEqual([
          inTwo.id,
        ]);
        const everywhere = await search("tantalum");
        expect([...hitIds(everywhere)].sort()).toEqual(
          [inOne.id, inTwo.id].sort(),
        );
        expect(hitIds(await search("tantalum", { limit: 1 })).length).toBe(1);
      } finally {
        close?.();
      }
    });

    it("excludes internal and placeholder records, and finds a placeholder once it stops being one", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const chat = await store.conversations.createChat({});
        const visible = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "the ferrite bead question",
        });
        await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "ferrite bead tool call bookkeeping",
          metadata: { internal: true },
        });
        const placeholder = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "ferrite bead partial answer",
          metadata: { placeholder: true },
        });

        expect(hitIds(await search("ferrite"))).toEqual([visible.id]);

        // The flags are read at QUERY time, not baked into an index: a
        // placeholder that finished streaming becomes findable, which an index
        // that had decided at insert time could never do.
        await store.conversations.updateMessage(placeholder.id, {
          content: "ferrite bead, finished answer",
          metadata: {},
        });
        expect([...hitIds(await search("ferrite"))].sort()).toEqual(
          [visible.id, placeholder.id].sort(),
        );
      } finally {
        close?.();
      }
    });

    it("finds a message on a branch the conversation moved away from", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const chat = await store.conversations.createChat({});
        const question = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "which package?",
        });
        const abandoned = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "a QFN48 package, probably",
        });
        // A second answer under the same question makes the first inactive.
        await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "actually a BGA",
          parentMessageId: question.id,
        });
        expect(
          (await store.conversations.listMessages(chat.id)).map((m) => m.id),
        ).not.toContain(abandoned.id);

        // Still a hit: "where did I see that?" is exactly a question about the
        // answers the conversation is no longer giving.
        expect(hitIds(await search("QFN48"))).toEqual([abandoned.id]);
      } finally {
        close?.();
      }
    });

    it("leaves no residue behind a deleted chat", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const doomed = await store.conversations.createChat({});
        const kept = await store.conversations.createChat({});
        await store.conversations.appendMessage({
          chatId: doomed.id,
          role: "user",
          content: "a memorable magnetometer",
        });
        const survivor = await store.conversations.appendMessage({
          chatId: kept.id,
          role: "user",
          content: "another magnetometer entirely",
        });
        expect((await search("magnetometer")).length).toBe(2);

        await store.conversations.deleteChat(doomed.id);

        // A hit pointing at a deleted chat is worse than a missing hit: a
        // client cannot tell it from a permissions bug.
        expect(hitIds(await search("magnetometer"))).toEqual([survivor.id]);
      } finally {
        close?.();
      }
    });

    it("survives a query made of query-language punctuation instead of raising", async () => {
      const { store, capabilities, close } = await create();
      try {
        if (capabilities?.search === false) return;
        const search = searchOf(store, expect);
        const chat = await store.conversations.createChat({});
        await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "a b c",
        });
        // Every character here is an FTS5 operator; handed over raw it is a
        // syntax error out of a method that documents none.
        expect(Array.isArray(await search('"a* (b^'))).toBe(true);
        expect(Array.isArray(await search("NOT AND OR"))).toBe(true);
        expect(Array.isArray(await search("c++ (2)"))).toBe(true);
        // Nothing left after sanitizing is no results, not an error.
        expect(await search("***")).toEqual([]);
        expect(await search("   ")).toEqual([]);
        expect(await search("")).toEqual([]);
      } finally {
        close?.();
      }
    });
  });
}
