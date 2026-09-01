/**
 * `useProposals`: the three verbs, and the reason the hook subscribes to the
 * chat topic at all.
 *
 * A proposal is staged by a TOOL, mid-run, and no click in the review pane
 * causes one to exist. The test that matters here is therefore the one where a
 * proposal is staged behind the hook's back and a finished run — not a
 * `reload()` — is what makes it appear.
 */
import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { createAgentKitClient, type AgentKitClient } from "@agentkit/client";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useChat, useProposals } from "../src/index.js";
import { wrapper } from "./support/render.js";
import {
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;
let client: AgentKitClient;

beforeEach(async () => {
  server = await startTestServer();
  client = createAgentKitClient({ baseUrl: server.baseUrl });
});

afterEach(async () => {
  await server.stop();
});

/** Stage one pending proposal the way a write tool would. */
async function stage(overrides: { actionId?: string } = {}) {
  return server.proposals.stage({
    chatId: TEST_CHAT_ID,
    scopeKey: "doc:1",
    toolName: "document.edit",
    kind: "document.edits",
    risk: "medium",
    operations: [{ op: "insert", at: 0, text: "hello" }],
    ...overrides,
  });
}

describe("useProposals", () => {
  test("lists what is staged", async () => {
    const staged = await stage();
    const { result } = renderHook(() => useProposals(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.proposals).toHaveLength(1));
    expect(result.current.proposals[0]?.id).toBe(staged.id);
    expect(result.current.proposals[0]?.status).toBe("pending");
    expect(result.current.proposals[0]?.toolName).toBe("document.edit");
    expect(result.current.error).toBeNull();
  });

  test("approve then apply walks the record to applied", async () => {
    const staged = await stage();
    const { result } = renderHook(() => useProposals(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.proposals).toHaveLength(1));

    await act(async () => {
      const decided = await result.current.approve(staged.id, "looks right");
      expect(decided?.status).toBe("approved");
      expect(decided?.decision?.actor).toBe("user");
    });
    expect(result.current.proposals[0]?.status).toBe("approved");
    expect(result.current.busy).toBe(false);
    expect(result.current.error).toBeNull();

    await act(async () => {
      const applied = await result.current.apply(staged.id);
      expect(applied?.status).toBe("applied");
      expect(applied?.outcome?.appliedOps).toBe(1);
    });
    expect(result.current.proposals[0]?.status).toBe("applied");
  });

  test("reject records the decision and the reason", async () => {
    const staged = await stage();
    const { result } = renderHook(() => useProposals(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.proposals).toHaveLength(1));

    await act(async () => {
      await result.current.reject(staged.id, "not now");
    });
    expect(result.current.proposals[0]?.status).toBe("rejected");
    expect(result.current.proposals[0]?.decision?.reason).toBe("not now");
  });

  test("a proposal that does not exist is a typed 404 in state", async () => {
    const { result } = renderHook(() => useProposals(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.loading).toBe(false));

    await act(async () => {
      const answer = await result.current.approve("prop-nope");
      expect(answer).toBeNull();
    });
    expect(result.current.busy).toBe(false);
    expect(result.current.error).toMatchObject({
      status: 404,
      code: "not_found",
    });
  });

  test("a finished run refreshes the queue without anyone calling reload", async () => {
    // Both hooks under ONE provider: the bus is what connects them.
    const { result } = renderHook(
      () => ({
        chat: useChat(TEST_CHAT_ID),
        proposals: useProposals(TEST_CHAT_ID),
      }),
      { wrapper: wrapper(client) },
    );
    await waitFor(() => expect(result.current.chat.status).toBe("idle"));
    expect(result.current.proposals.proposals).toEqual([]);

    // Staged behind the hook's back, the way a write tool would mid-run.
    const staged = await stage();
    expect(result.current.proposals.proposals).toEqual([]);

    await act(async () => {
      await result.current.chat.submit("do the thing");
    });
    await waitFor(() =>
      expect(result.current.proposals.proposals).toHaveLength(1),
    );
    expect(result.current.proposals.proposals[0]?.id).toBe(staged.id);
  });

  test("useProposals(null) is inert", () => {
    const { result } = renderHook(() => useProposals(null), {
      wrapper: wrapper(client),
    });
    expect(result.current.proposals).toEqual([]);
    expect(result.current.loading).toBe(false);
  });
});
