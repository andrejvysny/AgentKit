// `importConversation`: the history-migration primitive, graded on the two
// things that make it one — the ids come back unchanged, and the chat it writes
// is the same SHAPE every other operation in this port produces.
//
// The shape half deliberately reuses `assertOneActiveChain` from the tree
// driver rather than re-checking by hand: an import is a new way to create a
// chat, so the only interesting question about the chat it creates is whether
// the existing invariant holds over it. A second, subtly weaker check written
// next door would be the bug.
//
// FRAMEWORK-NEUTRAL, same rules as the rest of this package: no runner import
// and error assertions match on the `code` string rather than on `instanceof`.
// The one VALUE imported from `@agentkit/host` is `orderMessagesForProvider`,
// and deliberately so: the point of round-tripping tool linkage through an
// import is that the host's own replay ordering can still use it, and a
// re-implementation of that function here would grade a copy of the rule
// instead of the rule. `@agentkit/host` is a peer dependency of this package
// and a real dependency of every adapter that runs this suite.
import { orderMessagesForProvider } from "@agentkit/host";
import type {
  ImportConversationInput,
  ImportMessageInput,
  MessageRecord,
} from "@agentkit/host";
import {
  expectRejectsWithCode,
  type AssistantStoreConformanceHarness,
  type AssistantStoreConformanceTestApi,
} from "./conformance-support.js";
import { assertOneActiveChain } from "./conversation-tree-driver.js";

export interface ConversationImportOptions {
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/**
 * A conversation with everything an import has to carry that a loop of appends
 * could not build: two branches under one question, a run's internal records
 * chained onto the answer the user sees, and an active path that ends on the
 * SECOND branch.
 *
 * ```
 * u1                                    (root, active)
 * ├── a1        branchIndex 0           the abandoned answer
 * │   └── t1    internal                its tool result, chained
 * └── a2        branchIndex 1, active   the answer the chat is showing
 *     ├── t2    internal, active        its tool result, chained
 *     └── u2    (NOT here — a2's chain ends at t2)
 * ```
 *
 * Active: `u1 → a2 → t2`. Everything under `a1` is history the conversation
 * moved away from, which is exactly the state no sequence of appends can
 * reconstruct after the fact.
 */
function branchyFixture(chatId: string): ImportConversationInput {
  const messages: ImportMessageInput[] = [
    {
      id: "im-u1",
      role: "user",
      content: "what is the weather?",
      active: true,
    },
    {
      id: "im-a1",
      role: "assistant",
      content: "let me check",
      parentMessageId: "im-u1",
      active: false,
    },
    {
      id: "im-t1",
      role: "tool",
      content: '{"ok":false}',
      parentMessageId: "im-a1",
      active: false,
      internal: true,
    },
    {
      id: "im-a2",
      role: "assistant",
      content: "checking again",
      parentMessageId: "im-u1",
      active: true,
      metadata: { retry: 1 },
    },
    {
      id: "im-t2",
      role: "tool",
      content: '{"ok":true}',
      parentMessageId: "im-a2",
      active: true,
      internal: true,
    },
  ];
  return {
    chat: {
      id: chatId,
      title: "Migrated",
      metadata: { source: "legacy" },
      createdAt: "2024-01-02T03:04:05.000Z",
    },
    messages,
  };
}

export function describeConversationImport(
  options: ConversationImportOptions,
): void {
  const { create, test } = options;
  const { describe, it, expect } = test;

  describe("conversation import", () => {
    it("imports a branchy conversation with the caller's ids, store-assigned orderKey/depth/branchIndex, and a well-formed active path", async () => {
      const { store, close } = await create();
      try {
        const input = branchyFixture("chat-migrated-1");
        const chat = await store.conversations.importConversation(input);

        // The id is the whole point: a migration's links, bookmarks and
        // foreign keys already name it.
        expect(chat.id).toBe("chat-migrated-1");
        expect(chat.title).toBe("Migrated");
        expect(chat.metadata).toEqual({ source: "legacy" });
        expect(chat.archived).toBe(false);
        expect(chat.createdAt).toBe("2024-01-02T03:04:05.000Z");
        // An import preserves the chat's place in the listing rather than
        // jumping a year-old conversation to the top.
        expect(chat.updatedAt).toBe("2024-01-02T03:04:05.000Z");
        expect((await store.conversations.getChat("chat-migrated-1"))?.id).toBe(
          "chat-migrated-1",
        );

        // The path the import declared, and nothing else.
        const path = await store.conversations.listMessages(chat.id);
        expect(path.map((m) => m.id)).toEqual(["im-u1", "im-a2", "im-t2"]);
        // Graded by the SAME check the random-walk driver applies after every
        // one of its steps.
        assertOneActiveChain(
          path,
          new Map([
            ["im-u1", 2],
            ["im-a1", 1],
            ["im-a2", 1],
          ]),
          "imported branchy fixture",
          expect,
        );

        // Derived fields are the store's, assigned by the same sibling rules
        // an append follows: two answers to one question take 0 and 1.
        const answers = await store.conversations.listSiblings("im-a1");
        expect(answers.map((m) => m.id)).toEqual(["im-a1", "im-a2"]);
        expect(answers.map((m) => m.branchIndex)).toEqual([0, 1]);
        expect(answers.map((m) => m.depth)).toEqual([1, 1]);
        // Creation order, 1..n, whatever the tree looks like.
        const all = await store.conversations.listSiblings("im-u1");
        expect(all.map((m) => m.orderKey)).toEqual([1]);
        expect(path.map((m) => m.orderKey)).toEqual([1, 4, 5]);

        // The abandoned branch is stored, inactive, and reachable — an import
        // that quietly dropped it would look identical from `listMessages`.
        const abandoned = await store.conversations.listSiblings("im-t1");
        expect(abandoned.map((m) => m.id)).toEqual(["im-t1"]);
        expect(abandoned[0]?.active).toBe(false);

        // `internal: true` lands in metadata beside whatever the caller sent.
        expect(abandoned[0]?.metadata).toEqual({ internal: true });
        const active = path[1];
        expect(active?.metadata).toEqual({ retry: 1 });
        expect(path[2]?.metadata).toEqual({ internal: true });
      } finally {
        close?.();
      }
    });

    it("round-trips tool linkage VERBATIM, so an imported run still replays in provider order", async () => {
      const { store, close } = await create();
      try {
        const toolCalls = [
          {
            id: "call-1",
            name: "weather.lookup",
            argumentsJson: '{"city":"Brno"}',
          },
        ];
        const chat = await store.conversations.importConversation({
          chat: { id: "chat-tool-linkage" },
          messages: [
            { id: "tl-u1", role: "user", content: "weather?", active: true },
            // The turn that ASKED — replay-only, and useless to a provider
            // without the ids it declared.
            {
              id: "tl-a1",
              role: "assistant",
              content: "",
              parentMessageId: "tl-u1",
              active: true,
              internal: true,
              toolCalls,
            },
            // The result ANSWERING it: linked by id, not by adjacency.
            {
              id: "tl-t1",
              role: "tool",
              content: '{"tempC":21,"source":"…"}',
              parentMessageId: "tl-a1",
              active: true,
              internal: true,
              toolCallId: "call-1",
              modelResultJson: '{"tempC":21}',
            },
            {
              id: "tl-a2",
              role: "assistant",
              content: "21°C and clear.",
              parentMessageId: "tl-t1",
              active: true,
            },
          ],
        });

        const path = await store.conversations.listMessages(chat.id);
        expect(path.map((m) => m.id)).toEqual([
          "tl-u1",
          "tl-a1",
          "tl-t1",
          "tl-a2",
        ]);
        const byId = new Map(path.map((m) => [m.id, m] as const));
        // Verbatim: the store persists these three and derives nothing from
        // them.
        expect(byId.get("tl-a1")?.toolCalls).toEqual(toolCalls);
        expect(byId.get("tl-t1")?.toolCallId).toBe("call-1");
        expect(byId.get("tl-t1")?.modelResultJson).toBe('{"tempC":21}');
        // And invents none of them for the messages that carry none.
        expect(byId.get("tl-u1")?.toolCalls).toBe(undefined);
        expect(byId.get("tl-u1")?.toolCallId).toBe(undefined);
        expect(byId.get("tl-a2")?.modelResultJson).toBe(undefined);

        // THE POINT OF PERSISTING IT. The host's own replay ordering groups an
        // internal assistant turn with the results answering the ids IT
        // declared — so an import that dropped the linkage would migrate a
        // conversation whose every replay hands the provider a tool result with
        // no preceding `tool_calls`, which providers reject outright.
        const ordered: MessageRecord[] = orderMessagesForProvider(path);
        const assistantAt = ordered.findIndex((m) => m.id === "tl-a1");
        expect(assistantAt).toBeGreaterThan(-1);
        expect(ordered[assistantAt + 1]?.id).toBe("tl-t1");
      } finally {
        close?.();
      }
    });

    it("imports an empty conversation, and the chat then behaves like any other", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.importConversation({
          chat: { id: "chat-empty", archived: true },
          messages: [],
        });
        expect(chat.archived).toBe(true);
        expect(
          (await store.conversations.listMessages("chat-empty")).length,
        ).toBe(0);
        // An imported chat is a chat: appending to it starts its tree at 1.
        const first = await store.conversations.appendMessage({
          chatId: "chat-empty",
          role: "user",
          content: "hello",
        });
        expect(first.orderKey).toBe(1);
        expect(first.depth).toBe(0);
      } finally {
        close?.();
      }
    });

    it("an imported chat keeps growing: an append continues the active path from where the import left it", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.importConversation(
          branchyFixture("chat-migrated-2"),
        );
        const next = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "and tomorrow?",
        });
        // Hung off the imported leaf, with the numbering the import ended on.
        expect(next.parentMessageId).toBe("im-t2");
        expect(next.orderKey).toBe(6);
        expect(next.depth).toBe(3);
        const path = await store.conversations.listMessages(chat.id);
        expect(path.map((m) => m.id)).toEqual([
          "im-u1",
          "im-a2",
          "im-t2",
          next.id,
        ]);
      } finally {
        close?.();
      }
    });

    // Every rejection, and after each one the store must be exactly as it was:
    // an import is all-or-nothing, and a half-written conversation is worse
    // than a failed one because nothing says which half landed.
    const invalid: {
      name: string;
      reason: string;
      build: (chatId: string) => ImportConversationInput;
    }[] = [
      {
        name: "two active children of one message (not a path)",
        reason: "broken_active_chain",
        build: (id) => ({
          chat: { id },
          messages: [
            { id: "x-u1", role: "user", content: "q", active: true },
            {
              id: "x-a1",
              role: "assistant",
              content: "a1",
              parentMessageId: "x-u1",
              active: true,
            },
            {
              id: "x-a2",
              role: "assistant",
              content: "a2",
              parentMessageId: "x-u1",
              active: true,
            },
          ],
        }),
      },
      {
        name: "an active message under an inactive parent (a broken chain)",
        reason: "broken_active_chain",
        build: (id) => ({
          chat: { id },
          messages: [
            { id: "x-u1", role: "user", content: "q", active: false },
            {
              id: "x-a1",
              role: "assistant",
              content: "a",
              parentMessageId: "x-u1",
              active: true,
            },
          ],
        }),
      },
      {
        name: "an active path that stops above a childless leaf",
        reason: "active_leaf_has_child",
        build: (id) => ({
          chat: { id },
          messages: [
            { id: "x-u1", role: "user", content: "q", active: true },
            {
              id: "x-a1",
              role: "assistant",
              content: "a",
              parentMessageId: "x-u1",
              active: false,
            },
          ],
        }),
      },
      {
        name: "a parent that appears later in the list",
        reason: "forward_parent",
        build: (id) => ({
          chat: { id },
          messages: [
            {
              id: "x-a1",
              role: "assistant",
              content: "a",
              parentMessageId: "x-u1",
              active: true,
            },
            { id: "x-u1", role: "user", content: "q", active: true },
          ],
        }),
      },
      {
        name: "a parent the payload does not contain at all",
        reason: "unknown_parent",
        build: (id) => ({
          chat: { id },
          messages: [
            {
              id: "x-a1",
              role: "assistant",
              content: "a",
              parentMessageId: "x-ghost",
              active: true,
            },
          ],
        }),
      },
      {
        name: "the same message id twice",
        reason: "duplicate_message_id",
        build: (id) => ({
          chat: { id },
          messages: [
            { id: "x-u1", role: "user", content: "q", active: true },
            {
              id: "x-u1",
              role: "assistant",
              content: "a",
              parentMessageId: "x-u1",
              active: false,
            },
          ],
        }),
      },
      {
        name: "messages with nothing active",
        reason: "no_active_path",
        build: (id) => ({
          chat: { id },
          messages: [{ id: "x-u1", role: "user", content: "q", active: false }],
        }),
      },
    ];

    for (const testCase of invalid) {
      it(`rejects ${testCase.name} as invalid_import, writing nothing`, async () => {
        const { store, close } = await create();
        try {
          const chatId = "chat-bad";
          await expectRejectsWithCode(
            store.conversations.importConversation(testCase.build(chatId)),
            "invalid_import",
            expect,
          );
          // All-or-nothing: not even the chat row.
          expect(await store.conversations.getChat(chatId)).toBeNull();
          expect((await store.conversations.listChats()).length).toBe(0);
          // And the rejection says WHICH rule was broken, so an importer
          // fixing a thousand-message payload knows where to look.
          let reason: unknown;
          try {
            await store.conversations.importConversation(
              testCase.build(chatId),
            );
          } catch (err) {
            reason = (err as { details?: { reason?: unknown } }).details
              ?.reason;
          }
          expect(reason).toBe(testCase.reason);
        } finally {
          close?.();
        }
      });
    }

    it("rejects an import onto a chat id that already exists, leaving the existing chat untouched", async () => {
      const { store, close } = await create();
      try {
        const existing = await store.conversations.createChat({
          id: "chat-taken",
          title: "The real one",
        });
        await store.conversations.appendMessage({
          chatId: existing.id,
          role: "user",
          content: "mine",
        });
        await expectRejectsWithCode(
          store.conversations.importConversation(branchyFixture("chat-taken")),
          "invalid_import",
          expect,
        );
        // Not overwritten, not merged into: an id collision is a refusal.
        const after = await store.conversations.getChat("chat-taken");
        expect(after?.title).toBe("The real one");
        expect(
          (await store.conversations.listMessages("chat-taken")).map(
            (m) => m.content,
          ),
        ).toEqual(["mine"]);
      } finally {
        close?.();
      }
    });
  });
}
