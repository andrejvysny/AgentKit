/**
 * `useProviders`, and the one property worth a test of its own: the API key
 * this hook is handed never reaches this hook's state.
 *
 * `apiKey` is write-only in the contract — the server puts it in a `SecretStore`
 * under a ref and publishes the ref — so the assertion is not "we remembered to
 * strip it" but "there is nothing to strip". It is checked against the whole
 * serialised state rather than a named field, because the failure this guards
 * against is a key surviving somewhere nobody thought to look.
 */
import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { createAgentKitClient, type AgentKitClient } from "@agentkit/client";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useProviders } from "../src/index.js";
import { wrapper } from "./support/render.js";
import { startTestServer, type TestServer } from "./support/server.js";

const SECRET = "sk-test-must-not-be-rendered-9f3a";

let server: TestServer;
let client: AgentKitClient;

beforeEach(async () => {
  server = await startTestServer();
  client = createAgentKitClient({ baseUrl: server.baseUrl });
});

afterEach(async () => {
  await server.stop();
});

describe("useProviders", () => {
  test("lists the seeded provider on mount", async () => {
    const { result } = renderHook(() => useProviders(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.providers).toHaveLength(1));
    expect(result.current.providers[0]?.id).toBe("p1");
    expect(result.current.loading).toBe(false);
    expect(result.current.error).toBeNull();
  });

  test("create round-trips, and the apiKey is nowhere in the state", async () => {
    const { result } = renderHook(() => useProviders(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.providers).toHaveLength(1));

    await act(async () => {
      const created = await result.current.create({
        label: "Second",
        kind: "openai-compatible",
        baseUrl: "http://localhost:4321",
        defaultModel: "m2",
        apiKey: SECRET,
      });
      expect(created?.label).toBe("Second");
      expect(JSON.stringify(created)).not.toContain(SECRET);
    });

    expect(result.current.providers).toHaveLength(2);
    expect(result.current.busy).toBe(false);
    // The whole hook return value, not just the provider row.
    expect(JSON.stringify(result.current)).not.toContain(SECRET);
    // It went to the one place built to hold it.
    expect([...server.secrets.values.values()]).toContain(SECRET);
  });

  test("update and delete round-trip", async () => {
    const { result } = renderHook(() => useProviders(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.providers).toHaveLength(1));

    let createdId = "";
    await act(async () => {
      const created = await result.current.create({
        label: "Temporary",
        kind: "openai-compatible",
        baseUrl: "http://localhost:4321",
        defaultModel: "m2",
      });
      createdId = created!.id;
    });

    await act(async () => {
      const renamed = await result.current.update(createdId, {
        label: "Renamed",
        enabled: false,
      });
      expect(renamed?.label).toBe("Renamed");
      expect(renamed?.enabled).toBe(false);
    });
    expect(
      result.current.providers.find((p) => p.id === createdId)?.label,
    ).toBe("Renamed");

    await act(async () => {
      expect(await result.current.remove(createdId)).toBe(true);
    });
    expect(result.current.providers.map((p) => p.id)).toEqual(["p1"]);
  });

  test("loadModels files the catalogue under the provider id", async () => {
    const { result } = renderHook(() => useProviders(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.providers).toHaveLength(1));
    expect(result.current.models).toEqual({});

    await act(async () => {
      const models = await result.current.loadModels("p1");
      expect(models?.map((m) => m.modelId)).toEqual(["m1"]);
    });
    expect(result.current.models["p1"]?.map((m) => m.modelId)).toEqual(["m1"]);
  });

  test("a host wired without provider probing reports the 501, it does not throw", async () => {
    const { result } = renderHook(() => useProviders(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.providers).toHaveLength(1));

    // This fixture has no `providerOps`, so `refreshModels` and `test` are 501
    // — the route exists in the contract and this deployment cannot serve it.
    await act(async () => {
      expect(await result.current.refreshModels("p1")).toBeNull();
    });
    expect(result.current.busy).toBe(false);
    expect(result.current.error).toMatchObject({ status: 501 });

    await act(async () => {
      expect(await result.current.test("p1")).toBeNull();
    });
    expect(result.current.error).toMatchObject({ status: 501 });
  });

  test("a provider that does not exist is a typed 404 in state", async () => {
    const { result } = renderHook(() => useProviders(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.providers).toHaveLength(1));

    await act(async () => {
      expect(await result.current.update("nope", { label: "x" })).toBeNull();
    });
    expect(result.current.error).toMatchObject({
      status: 404,
      code: "not_found",
    });
  });
});
