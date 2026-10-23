import "../../react/tests/support/dom.js";
import { expect, test } from "bun:test";
import { HangingProviderClient } from "@agentkit/testing";
import { startProviderBridge } from "./support/provider-bridge.js";

test("provider fixture shutdown settles callbacks and closes the listener", async () => {
  const provider = new HangingProviderClient();
  const bridge = startProviderBridge(provider);
  const response = await fetch(bridge.url, {
    method: "POST",
    headers: {
      authorization: `Bearer ${bridge.token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      runId: "fixture-shutdown",
      model: "m",
      messages: [],
    }),
  });
  const reader = response.body!.getReader();
  try {
    await reader.read();
    await provider.whenBlocking();
    expect(bridge.activeCallbacks).toBe(1);
    await bridge.stop();
    expect(bridge.activeCallbacks).toBe(0);
    await expect(fetch(bridge.url)).rejects.toThrow();
  } finally {
    await reader.cancel().catch(() => undefined);
    await bridge.stop();
  }
});
