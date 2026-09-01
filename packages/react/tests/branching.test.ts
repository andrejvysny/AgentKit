/**
 * Branching, which is where the emitter earns its place.
 *
 * A chat is a tree; `listMessages` reports one path through it; and the thing
 * that changes which path that is — `activateBranch` — is called by a DIFFERENT
 * hook than the one rendering the messages. So the tests here mount BOTH hooks
 * under ONE provider and assert on the one that was not called: `activate` is
 * the write, and `useChat.messages` changing is the proof the invalidation
 * reached it.
 *
 * Both under one provider is not incidental. The bus belongs to the provider,
 * so two `renderHook` calls are two trees, two providers and two buses — which
 * is the right isolation for a test and the wrong shape for this one.
 */
import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { createAgentKitClient, type AgentKitClient } from "@agentkit/client";
import { MockProviderClient } from "@agentkit/testing";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useBranches, useChat } from "../src/index.js";
import { strictWrapper, wrapper } from "./support/render.js";
import {
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;
let provider: MockProviderClient;
let client: AgentKitClient;

beforeEach(async () => {
  provider = new MockProviderClient();
  provider.setScript([{ steps: [{ kind: "text", content: "First answer." }] }]);
  server = await startTestServer({ provider });
  client = createAgentKitClient({ baseUrl: server.baseUrl });
});

afterEach(async () => {
  await server.stop();
});

interface Props {
  messageId: string | null;
  strict?: boolean;
}

/** Both hooks, one provider, one bus. `messageId` is re-rendered in. */
function mount(strict = false) {
  return renderHook(
    ({ messageId }: Props) => ({
      chat: useChat(TEST_CHAT_ID),
      branches: useBranches(messageId),
    }),
    {
      wrapper: strict ? strictWrapper(client) : wrapper(client),
      initialProps: { messageId: null } as Props,
    },
  );
}

/** Take the first turn and return the two message ids it created. */
async function firstTurn(view: ReturnType<typeof mount>) {
  await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));
  await act(async () => {
    await view.result.current.chat.submit("Say hello.");
  });
  await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));
  const [user, assistant] = view.result.current.chat.messages;
  return { userId: user!.id, assistantId: assistant!.id };
}

describe("regenerate", () => {
  test("adds a sibling, switches to it, and streams the new answer", async () => {
    const view = mount();
    const { assistantId } = await firstTurn(view);
    expect(view.result.current.chat.messages[1]?.content).toBe("First answer.");

    provider.setScript([
      { steps: [{ kind: "text", content: "Second answer." }] },
    ]);
    await act(async () => {
      await view.result.current.chat.regenerate(assistantId);
    });
    await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));

    // Two messages still: a sibling REPLACES the old answer on the path, it
    // does not append below it.
    expect(view.result.current.chat.messages).toHaveLength(2);
    expect(view.result.current.chat.messages[1]?.content).toBe(
      "Second answer.",
    );
    expect(view.result.current.chat.messages[1]?.id).not.toBe(assistantId);

    const siblings = await client.listSiblings({ messageId: assistantId });
    expect(siblings).toHaveLength(2);
    expect(siblings.map((m) => m.branchIndex)).toEqual([0, 1]);
  });
});

describe("editAndResubmit", () => {
  test("branches under the named message and cuts the path there", async () => {
    const view = mount();
    const { userId, assistantId } = await firstTurn(view);

    provider.setScript([
      { steps: [{ kind: "text", content: "Answer to the rewrite." }] },
    ]);
    await act(async () => {
      await view.result.current.chat.editAndResubmit(
        userId,
        "Say hello, again.",
      );
    });
    await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));

    const path = view.result.current.chat.messages;
    // The old question stays (it is the branch point), its old answer does not.
    expect(path.map((m) => m.id)).toContain(userId);
    expect(path.map((m) => m.id)).not.toContain(assistantId);
    expect(path.at(-1)?.content).toBe("Answer to the rewrite.");
    expect(path.at(-2)?.content).toBe("Say hello, again.");

    const page = await client.listMessages({ chatId: TEST_CHAT_ID });
    expect(path.map((m) => m.id)).toEqual(page.items.map((m) => m.id));
  });

  test("the rewritten question is on screen before the server answers", async () => {
    const view = mount();
    const { userId } = await firstTurn(view);

    act(() => {
      void view.result.current.chat.editAndResubmit(userId, "Rewritten.");
    });
    expect(view.result.current.chat.status).toBe("loading");
    // Truncated at the branch point, then the optimistic pair — the old answer
    // is already gone rather than sitting above the new question.
    expect(view.result.current.chat.messages.map((m) => m.content)).toEqual([
      "Say hello.",
      "Rewritten.",
      "",
    ]);

    await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));
  });
});

describe("useBranches", () => {
  test("counts the siblings and follows a regeneration through the bus", async () => {
    const view = mount();
    const { assistantId } = await firstTurn(view);

    view.rerender({ messageId: assistantId });
    await waitFor(() => expect(view.result.current.branches.count).toBe(1));
    expect(view.result.current.branches.index).toBe(0);

    provider.setScript([{ steps: [{ kind: "text", content: "Second." }] }]);
    await act(async () => {
      await view.result.current.chat.regenerate(assistantId);
    });

    // Nobody told `useBranches` to reload. The chat's terminal-run invalidation
    // is what makes the counter read "2 of 2".
    await waitFor(() => expect(view.result.current.branches.count).toBe(2));
    expect(view.result.current.branches.index).toBe(1);
  });

  test("activate switches the path and useChat follows through the bus", async () => {
    const view = mount();
    const { assistantId } = await firstTurn(view);
    provider.setScript([{ steps: [{ kind: "text", content: "Second." }] }]);
    await act(async () => {
      await view.result.current.chat.regenerate(assistantId);
    });
    await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));
    expect(view.result.current.chat.messages[1]?.content).toBe("Second.");

    view.rerender({ messageId: assistantId });
    await waitFor(() => expect(view.result.current.branches.count).toBe(2));

    // The BRANCH hook does the write; the CHAT hook is never called.
    await act(async () => {
      await view.result.current.branches.activate(assistantId);
    });

    await waitFor(() =>
      expect(view.result.current.chat.messages[1]?.id).toBe(assistantId),
    );
    expect(view.result.current.chat.messages[1]?.content).toBe("First answer.");
    expect(view.result.current.branches.index).toBe(0);
  });

  test("under <StrictMode> the doubled effect still loads exactly one list", async () => {
    const view = mount(true);
    const { assistantId } = await firstTurn(view);
    view.rerender({ messageId: assistantId, strict: true });
    await waitFor(() => expect(view.result.current.branches.count).toBe(1));
    expect(view.result.current.branches.siblings.map((m) => m.id)).toEqual([
      assistantId,
    ]);
    expect(view.result.current.branches.error).toBeNull();
  });

  test("useBranches(null) is inert", async () => {
    const view = mount();
    await waitFor(() => expect(view.result.current.chat.status).toBe("idle"));
    expect(view.result.current.branches.count).toBe(0);
    expect(view.result.current.branches.index).toBe(-1);
    expect(view.result.current.branches.siblings).toEqual([]);
  });
});
