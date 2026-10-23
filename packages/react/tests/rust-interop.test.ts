import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { expect, test } from "bun:test";
import { startCompatibilityServer } from "../../../rust/tests/support/http-server.js";
import { useChat, useProviders, useRun } from "../src/index.js";
import { strictWrapper, wrapper } from "./support/render.js";

for (const backend of ["typescript", "rust"] as const) {
  test(`same useChat assertions against ${backend}`, async () => {
    const server = await startCompatibilityServer(backend);
    const chat = await server.client.createChat({ title: "React native" });
    const mounted = renderHook(() => useChat(chat.id), {
      wrapper: wrapper(server.client),
    });
    try {
      await waitFor(() => expect(mounted.result.current.status).toBe("idle"));
      act(() => {
        void mounted.result.current.submit("Say hello.");
      });
      expect(
        mounted.result.current.messages.map((message) => message.role),
      ).toEqual(["user", "assistant"]);
      expect(mounted.result.current.messages[0]?.metadata["optimistic"]).toBe(
        true,
      );
      await waitFor(() => expect(mounted.result.current.status).toBe("idle"), {
        timeout: 10_000,
      });
      expect(mounted.result.current.phase).toBe("completed");
      expect(mounted.result.current.error).toBeNull();
      expect(mounted.result.current.messages[1]?.content).toBe(
        "Hello, client.",
      );
      const page = await server.client.listMessages({ chatId: chat.id });
      expect(
        mounted.result.current.messages.map((message) => message.id),
      ).toEqual(page.items.map((message) => message.id));
      expect(mounted.result.current.messages[1]?.metadata["placeholder"]).toBe(
        false,
      );
    } finally {
      mounted.unmount();
      await server.stop();
    }
  }, 60_000);

  test(`same StrictMode useRun and provider assertions against ${backend}`, async () => {
    const server = await startCompatibilityServer(backend);
    try {
      const chat = await server.client.createChat({});
      const submitted = await server.client.submitMessage(
        { chatId: chat.id },
        { content: "hello" },
      );
      const mounted = renderHook(() => useRun(submitted.result.runId), {
        wrapper: strictWrapper(server.client),
      });
      try {
        await waitFor(
          () => expect(mounted.result.current.phase).toBe("completed"),
          { timeout: 10_000 },
        );
        expect(mounted.result.current.events.map((event) => event.seq)).toEqual(
          mounted.result.current.events.map((_event, index) => index),
        );
        expect(
          mounted.result.current.events
            .filter((event) => event.type === "run.message.delta")
            .map((event) => event.data.delta)
            .join(""),
        ).toBe("Hello, client.");
      } finally {
        mounted.unmount();
      }
      const providers = renderHook(() => useProviders(), {
        wrapper: wrapper(server.client),
      });
      try {
        await waitFor(() =>
          expect(providers.result.current.loading).toBe(false),
        );
        expect(providers.result.current.error).toBeNull();
        await act(async () => {
          await providers.result.current.create({
            id: "hook-provider",
            label: "Hook",
            kind: "openai-compatible",
            baseUrl: "http://127.0.0.1:9999",
            defaultModel: "m1",
          });
        });
        expect(
          providers.result.current.providers.some(
            (provider) => provider.id === "hook-provider",
          ),
        ).toBe(true);
      } finally {
        providers.unmount();
      }
    } finally {
      await server.stop();
    }
  }, 60_000);
}
