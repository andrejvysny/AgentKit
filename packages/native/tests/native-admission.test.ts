import { expect, test } from "bun:test";
import {
  NativeManagedContent,
  listNativeProfiles,
  resolveNativeProfile,
  NativeStore,
  type ProviderRequest,
} from "../src/index.js";

test("native profile defaults and selected content reach the provider callback", async () => {
  const store = await NativeStore.inMemory();
  const managed = new NativeManagedContent({ store });
  const skill = await managed.skills.publish({
    source: "docs",
    path: "guide",
    content:
      "---\nname: guide\ndescription: guide\n---\nselected native instruction\n",
  });
  const service = await store.createService({
    profiles: [
      {
        id: "custom",
        extends: "chat",
        model: "profile-model",
        reasoningEffort: "high",
        limits: { maxOutputTokens: 13 },
      },
    ],
  });
  const seen: ProviderRequest[] = [];
  let authorityCalls = 0;
  service.setContentAuthority(() => {
    authorityCalls++;
    throw new Error("no connector authority needed");
  });
  service.setProvider((request) => {
    seen.push(request);
    return { content: "answer", finishReason: "stop" };
  });
  try {
    await expect(
      service.createSession({ profileId: "unknown" }),
    ).rejects.toThrow("unknown profile");
    await service.createSession({
      sessionId: "chat",
      profileId: "custom",
      skills: [{ id: skill.id, revision: skill.revision }],
    });
    const sent = await service.send("chat", { content: "question" });
    if (sent.kind !== "turn") throw new Error("expected turn");
    const stream = await service.subscribeTurn(sent.turnId);
    let done = false;
    for (let index = 0; index < 80 && !done; index++)
      done = (await stream.poll(64, 250)).done;
    expect(done).toBe(true);
    expect(seen).toHaveLength(1);
    expect(seen[0]?.model).toBe("profile-model");
    expect(seen[0]?.reasoningEffort).toBe("high");
    expect(seen[0]?.maxOutputTokens).toBe(13);
    expect(seen[0]?.tools).toEqual([]);
    expect(JSON.stringify(seen[0]?.runContent)).toContain(
      "selected native instruction",
    );
    expect(JSON.stringify(seen[0]?.messages)).not.toContain(
      "selected native instruction",
    );
    expect(authorityCalls).toBe(0);
  } finally {
    await service.close();
    await store.close();
  }
}, 20_000);

test("managed native provider identity is frozen for session and HTTP admission", async () => {
  const { startProviderMock } = await import("./helpers/loopback-provider.js");
  const provider = await startProviderMock();
  const store = await NativeStore.inMemory();
  const service = await store.createService();
  const vault = new Map<string, string>();
  const approvals: string[] = [];
  let fallbackCalls = 0;
  service.setProviderSecretStore({
    get: (reference) => vault.get(reference) ?? null,
    put: (reference, value) => {
      vault.set(reference, value);
    },
    delete: (reference) => {
      vault.delete(reference);
    },
  });
  const consent: import("../src/index.js").ProviderConsentHandler = (
    request,
  ) => {
    approvals.push(request.payloadJson);
    return { approved: true };
  };
  service.setProviderConsent(consent);
  service.setProvider(() => {
    fallbackCalls++;
    return { content: "callback answer", finishReason: "stop" };
  });
  const bearerToken = "native-provider-admission-token-32-bytes";
  try {
    const server = await service.startHttpServer({
      bearerToken,
      providerSecrets: "callback",
      providerOperations: "native",
    });
    const headers = {
      authorization: `Bearer ${bearerToken}`,
      "content-type": "application/json",
    };
    const created = await fetch(`${server.baseUrl}/v1/providers`, {
      method: "POST",
      headers,
      body: JSON.stringify({
        id: "managed",
        label: "managed",
        kind: "openai-compatible",
        baseUrl: provider.baseUrl,
        defaultModel: "loopback-model",
        apiKey: "synthetic-native-key",
      }),
    });
    expect(created.status, await created.clone().text()).toBe(201);
    expect(() =>
      service.setProviderSecretStore({
        get: () => null,
        put: () => {},
        delete: () => {},
      }),
    ).toThrow("already configured");
    expect(() =>
      service.setProviderConsent(() => ({ approved: true })),
    ).toThrow("already configured");
    service.setProviderConsent(null);
    expect(() =>
      service.setProviderConsent(() => ({ approved: true })),
    ).toThrow("already configured");
    service.setProviderConsent(consent);
    await service.createSession({
      sessionId: "native",
      profileId: "chat",
      providerId: "managed",
      model: "loopback-model",
    });
    const sent = await service.send("native", { content: "native question" });
    if (sent.kind !== "turn") throw new Error("expected turn");
    const subscription = await service.subscribeTurn(sent.turnId);
    let done = false;
    for (let index = 0; index < 80 && !done; index++)
      done = (await subscription.poll(64, 250)).done;
    expect(done).toBe(true);
    expect(provider.requests).toHaveLength(1);
    expect(approvals).toHaveLength(1);
    expect(fallbackCalls).toBe(0);
    expect(
      await store.callStore("tasks.getTask", { taskId: sent.turnId }),
    ).toMatchObject({
      status: "completed",
      payload: {
        providerSnapshot: {
          model: "loopback-model",
          connection: { id: "managed" },
        },
      },
    });
    const chatResponse = await fetch(`${server.baseUrl}/v1/chats`, {
      method: "POST",
      headers,
      body: "{}",
    });
    const chat = (await chatResponse.json()) as { id: string };
    const fallback = await fetch(
      `${server.baseUrl}/v1/chats/${chat.id}/messages`,
      {
        method: "POST",
        headers: { ...headers, "idempotency-key": "explicit-static" },
        body: JSON.stringify({
          content: "callback question",
          providerId: "js",
          model: "callback-model",
        }),
      },
    );
    expect(fallback.status, await fallback.clone().text()).toBe(201);
    const fallbackRun = (await fallback.json()) as { runId: string };
    const fallbackSubscription = await service.subscribeTurn(fallbackRun.runId);
    done = false;
    for (let index = 0; index < 80 && !done; index++)
      done = (await fallbackSubscription.poll(64, 250)).done;
    expect(done).toBe(true);
    expect(fallbackCalls).toBe(1);
    expect(provider.requests).toHaveLength(1);
  } finally {
    await service.close();
    await store.close();
    await provider.stop();
  }
}, 30_000);

test("pure native profile queries match direct session settings and template-only content", async () => {
  const overrides = [
    {
      id: "direct",
      extends: "chat",
      model: "profile-model",
      reasoningEffort: "high" as const,
    },
  ];
  const resolved = await resolveNativeProfile("direct", overrides);
  expect(
    (await listNativeProfiles(overrides)).find((item) => item.id === "direct"),
  ).toEqual(resolved);
  expect(resolved.tools).toEqual([]);
  expect(resolved.digest).toMatch(/^[a-f0-9]{64}$/);
  await expect(resolveNativeProfile("unknown")).rejects.toThrow(
    "unknown profile",
  );
  const store = await NativeStore.inMemory();
  const content = new NativeManagedContent({ store });
  await content.promptTemplates.publish({
    id: "template",
    revision: "1",
    description: "template-only",
    argumentsSchema: { type: "object", properties: {} },
    messages: [{ role: "system", content: "template-only native instruction" }],
  });
  const service = await store.createService({ profiles: overrides });
  const seen: ProviderRequest[] = [];
  service.setProvider((request) => {
    seen.push(request);
    return { content: "answer", finishReason: "stop" };
  });
  try {
    const session = await service.createSession({
      sessionId: "direct",
      profileId: "direct",
      model: "explicit-model",
      reasoningEffort: "low",
      contentSelection: {
        template: { id: "template", revision: "1", arguments: {} },
      },
    });
    expect(JSON.stringify(session)).toContain(resolved.digest);
    const sent = await service.send("direct", { content: "question" });
    if (sent.kind !== "turn") throw new Error("expected turn");
    const stream = await service.subscribeTurn(sent.turnId);
    let done = false;
    for (let index = 0; index < 80 && !done; index++)
      done = (await stream.poll(64, 250)).done;
    expect(done).toBe(true);
    expect(seen).toHaveLength(1);
    expect(seen[0]?.model).toBe("explicit-model");
    expect(seen[0]?.reasoningEffort).toBe("low");
    expect(JSON.stringify(seen[0]?.runContent)).toContain(
      "template-only native instruction",
    );
    expect(JSON.stringify(seen[0]?.messages)).not.toContain(
      "template-only native instruction",
    );
  } finally {
    await service.close();
    await store.close();
  }
}, 20_000);

test("custom provider checks native content authority after awaited preparation", async () => {
  const store = await NativeStore.inMemory();
  const content = new NativeManagedContent({ store });
  const skill = await content.skills.publish({
    source: "docs",
    path: "guard",
    content:
      "---\nname: guard\ndescription: guard\nmetadata:\n  requires-connectors: github\n---\nguarded instruction\n",
  });
  const service = await store.createService();
  let allowed = true;
  service.setContentAuthority(() => (allowed ? ["github"] : []));
  let signalEntered: () => void = () => {};
  const entered = new Promise<void>((resolve) => {
    signalEntered = resolve;
  });
  let release: () => void = () => {};
  const preparation = new Promise<void>((resolve) => {
    release = resolve;
  });
  let sends = 0;
  let check: (() => Promise<void>) | undefined;
  service.setProvider(async (request) => {
    check = request.checkAuthority;
    signalEntered();
    await preparation;
    await request.checkAuthority?.();
    sends++;
    return { content: "unsafe", finishReason: "stop" };
  });
  try {
    await service.createSession({
      sessionId: "guard",
      profileId: "chat",
      model: "callback-model",
      skills: [{ id: skill.id, revision: skill.revision }],
    });
    const sent = await service.send("guard", { content: "question" });
    if (sent.kind !== "turn") throw new Error("expected turn");
    await Promise.race([
      entered,
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error("provider callback timeout")), 5000),
      ),
    ]);
    allowed = false;
    release();
    const stream = await service.subscribeTurn(sent.turnId);
    let done = false;
    for (let index = 0; index < 80 && !done; index++)
      done = (await stream.poll(64, 250)).done;
    expect(done).toBe(true);
    expect(sends).toBe(0);
    expect(check).toBeDefined();
    await expect(check!()).rejects.toThrow("no longer valid");
    expect(
      await store.callStore("tasks.getTask", { taskId: sent.turnId }),
    ).toMatchObject({ status: "failed" });
  } finally {
    release();
    await service.close();
    await store.close();
  }
}, 20_000);
