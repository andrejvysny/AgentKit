/**
 * The provider, the client override, and the bus underneath both.
 *
 * The emitter is tested directly rather than only through the hooks because it
 * is the one piece here with rules of its own — a throwing subscriber must not
 * take out the others, and an unsubscribe must be safe to call twice.
 */
import "./support/dom.js";
import { renderHook, waitFor } from "@testing-library/react";
import { createAgentKitClient } from "@agentkit/client";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  chatTopic,
  createChangeEmitter,
  useAgentKitClient,
  useChat,
} from "../src/index.js";
import { wrapper } from "./support/render.js";
import {
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;

beforeEach(async () => {
  server = await startTestServer();
});

afterEach(async () => {
  await server.stop();
});

describe("AgentKitProvider", () => {
  test("a hook outside the provider throws an explanation, not a null", () => {
    // React logs the render error; the assertion is on what was thrown.
    expect(() => renderHook(() => useAgentKitClient())).toThrow(
      /no AgentKitProvider above this hook/,
    );
  });

  test("useAgentKitClient hands back the provider's client", async () => {
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const { result } = renderHook(() => useAgentKitClient(), {
      wrapper: wrapper(client),
    });
    expect(result.current).toBe(client);
  });

  test("an override replaces the client for one hook only", async () => {
    const provided = createAgentKitClient({ baseUrl: server.baseUrl });
    const second = await startTestServer();
    try {
      const override = createAgentKitClient({ baseUrl: second.baseUrl });
      await override.submitMessage(
        { chatId: TEST_CHAT_ID },
        { content: "on the other host" },
      );

      const { result } = renderHook(
        () => ({
          fromProvider: useChat(TEST_CHAT_ID),
          fromOverride: useChat(TEST_CHAT_ID, { client: override }),
        }),
        { wrapper: wrapper(provided) },
      );

      await waitFor(() =>
        expect(result.current.fromOverride.messages).toHaveLength(2),
      );
      // Same chat id, different host: the override really is talking elsewhere.
      expect(result.current.fromProvider.messages).toEqual([]);
      expect(result.current.fromOverride.messages[0]?.content).toBe(
        "on the other host",
      );
    } finally {
      await second.stop();
    }
  });
});

describe("the change emitter", () => {
  test("delivers to every subscriber of a topic and no other", () => {
    const emitter = createChangeEmitter();
    const seen: string[] = [];
    emitter.subscribe(chatTopic("a"), () => seen.push("a1"));
    emitter.subscribe(chatTopic("a"), () => seen.push("a2"));
    emitter.subscribe(chatTopic("b"), () => seen.push("b1"));

    emitter.emit(chatTopic("a"));
    expect(seen).toEqual(["a1", "a2"]);

    emitter.emit(chatTopic("c"));
    expect(seen).toEqual(["a1", "a2"]);
  });

  test("carries the origin so a hook can skip its own echo", () => {
    const emitter = createChangeEmitter();
    const origins: (string | undefined)[] = [];
    emitter.subscribe(chatTopic("a"), (event) => origins.push(event.origin));

    emitter.emit(chatTopic("a"), { origin: "chat-7" });
    emitter.emit(chatTopic("a"));
    expect(origins).toEqual(["chat-7", undefined]);
  });

  test("one throwing subscriber does not cost the others their event", () => {
    const emitter = createChangeEmitter();
    const seen: string[] = [];
    emitter.subscribe(chatTopic("a"), () => {
      throw new Error("subscriber bug");
    });
    emitter.subscribe(chatTopic("a"), () => seen.push("survived"));

    expect(() => emitter.emit(chatTopic("a"))).not.toThrow();
    expect(seen).toEqual(["survived"]);
  });

  test("unsubscribing is idempotent and stops delivery", () => {
    const emitter = createChangeEmitter();
    let calls = 0;
    const off = emitter.subscribe(chatTopic("a"), () => {
      calls += 1;
    });
    emitter.emit(chatTopic("a"));
    off();
    off();
    emitter.emit(chatTopic("a"));
    expect(calls).toBe(1);
  });

  test("a subscriber that unsubscribes mid-dispatch does not skip the next one", () => {
    const emitter = createChangeEmitter();
    const seen: string[] = [];
    const off = emitter.subscribe(chatTopic("a"), () => {
      seen.push("first");
      off();
    });
    emitter.subscribe(chatTopic("a"), () => seen.push("second"));

    emitter.emit(chatTopic("a"));
    expect(seen).toEqual(["first", "second"]);
  });
});
