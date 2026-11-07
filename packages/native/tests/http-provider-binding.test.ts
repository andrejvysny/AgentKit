import { expect, test } from "bun:test";
import { NativeSessionService, NativeStore } from "../src/index.js";
import { startProviderMock } from "./helpers/loopback-provider.js";

const TOKEN = "12345678901234567890123456789012";

async function drain(
  service: NativeSessionService,
  runId: string,
): Promise<void> {
  const stream = await service.subscribeTurn(runId);
  try {
    for (let index = 0; index < 80; index++) {
      if ((await stream.poll(64, 250)).done) return;
    }
    throw new Error("HTTP provider turn did not settle.");
  } finally {
    stream.cancel();
  }
}

async function lane(providerId: string) {
  const provider = await startProviderMock();
  const connectionId = await NativeSessionService.registerProviderConnection(
    provider.baseUrl,
  );
  return {
    provider,
    config: {
      protocol: "chat-completions" as const,
      providerId,
      connectionId,
      model: "loopback-model",
    },
  };
}

async function fixture() {
  const [primary, alternate] = await Promise.all([
    lane("http-static"),
    lane("http-alternate"),
  ]);
  const store = await NativeStore.inMemory();
  const service = await store.createService({
    providerConfig: primary.config,
    additionalProviders: [alternate.config],
  });
  await service.createSession({ sessionId: "bound", profileId: "chat" });
  const { baseUrl } = await service.startHttpServer({
    bearerToken: TOKEN,
    defaultModel: "loopback-model",
  });
  return {
    store,
    service,
    primary: primary.provider,
    alternate: alternate.provider,
    send: (providerId: string) =>
      fetch(`${baseUrl}/v1/chats/bound/messages`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${TOKEN}`,
          "content-type": "application/json",
          "idempotency-key": providerId,
        },
        body: JSON.stringify({ content: "question", providerId }),
      }),
    async close() {
      await service.close();
      await store.close();
      await primary.provider.stop();
      await alternate.provider.stop();
    },
  };
}

test("HTTP binds configured static providers and refuses other ids before writes", async () => {
  const fx = await fixture();
  try {
    const wrong = await fx.send("other");
    expect(wrong.status).toBe(400);
    expect(await wrong.json()).toMatchObject({ code: "provider_not_bound" });
    expect(
      await fx.store.callStore<unknown[]>("tasks.listByScope", {
        scopeId: "bound",
      }),
    ).toEqual([]);
    expect(
      await fx.store.callStore<unknown[]>("conversations.listMessages", {
        chatId: "bound",
      }),
    ).toEqual([]);
    expect(fx.primary.requests).toHaveLength(0);
    expect(fx.alternate.requests).toHaveLength(0);
    for (const providerId of ["http-static", "http-alternate"]) {
      const response = await fx.send(providerId);
      expect(response.status, await response.clone().text()).toBe(201);
      const { runId } = (await response.json()) as { runId: string };
      await drain(fx.service, runId);
      expect(
        await fx.store.callStore("tasks.getTask", { taskId: runId }),
      ).toMatchObject({ status: "completed", payload: { providerId } });
    }
    expect(fx.primary.requests).toHaveLength(1);
    expect(fx.alternate.requests).toHaveLength(1);
  } finally {
    await fx.close();
  }
}, 20_000);
