import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type {
  ProviderConsentRequest,
  ProviderSecretStore,
} from "@agentkit/native";
import { buildApp, BASE_PATH, type App } from "../src/wiring.js";

const apps: App[] = [];
const servers: ReturnType<typeof Bun.serve>[] = [];
const dirs: string[] = [];
afterEach(async () => {
  for (const app of apps.splice(0)) await app.stop();
  for (const server of servers.splice(0)) await server.stop(true);
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});

async function call(
  app: App,
  path: string,
  method = "GET",
  body?: unknown,
): Promise<Response> {
  return app.fetch(
    new Request(`http://localhost${BASE_PATH}/v1${path}`, {
      method,
      headers: {
        "content-type": "application/json",
        "idempotency-key": crypto.randomUUID(),
      },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    }),
  );
}

function loopbackProvider(seen: string[]) {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      seen.push(request.headers.get("authorization") ?? "missing");
      if (request.url.endsWith("/models"))
        return Response.json({ data: [{ id: "managed-model" }] });
      const body = (await request.json()) as { model: string };
      const chunk = (delta: object, finish: string | null) =>
        `data: ${JSON.stringify({
          id: "managed",
          object: "chat.completion.chunk",
          model: body.model,
          choices: [{ index: 0, delta, finish_reason: finish }],
        })}\n\n`;
      return new Response(
        chunk({ content: "managed native answer" }, null) +
          chunk({}, "stop") +
          "data: [DONE]\n\n",
        { headers: { "content-type": "text/event-stream" } },
      );
    },
  });
  servers.push(server);
  return server;
}

async function turn(app: App): Promise<string> {
  const created = await call(app, "/chats", "POST", {});
  expect(created.status).toBe(201);
  const chat = (await created.json()) as { id: string };
  const result = await call(app, `/chats/${chat.id}/messages`, "POST", {
    content: "hello",
  });
  expect(result.status).toBe(201);
  const submitted = (await result.json()) as { runId: string };
  const stream = await call(app, `/runs/${submitted.runId}/stream`);
  const frames = await stream.text();
  expect(frames).toContain("event: run.completed");
  expect(frames).toContain("managed native answer");
  return chat.id;
}

test("managed mode refuses missing host ports without opening a store", async () => {
  await expect(buildApp({ dbPath: ":memory:", env: {} })).rejects.toThrow(
    "requires explicit durable secret and consent ports",
  );
});

test("managed native metadata and host vault survive restart; consent gates dispatch", async () => {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-managed-example-"));
  dirs.push(dir);
  const dbPath = join(dir, "native.sqlite");
  // The test host vault outlives services. Production hosts supply durable encrypted storage.
  const values = new Map<string, string>();
  const secrets: ProviderSecretStore = {
    get: (reference) => values.get(reference) ?? null,
    put: (reference, value) => {
      values.set(reference, value);
    },
    delete: (reference) => {
      values.delete(reference);
    },
  };
  let approved = true;
  const approvals: ProviderConsentRequest[] = [];
  const consent = (request: ProviderConsentRequest) => {
    approvals.push(request);
    return { approved };
  };
  const seen: string[] = [];
  const provider = loopbackProvider(seen);
  const env = {
    AGENTKIT_BASE_URL: `http://127.0.0.1:${provider.port}/v1`,
    AGENTKIT_MODEL: "managed-model",
    AGENTKIT_API_KEY: "host-only-secret",
  };
  const first = await buildApp({
    dbPath,
    env,
    providerMode: "managed",
    providerPorts: { secrets, consent },
  });
  apps.push(first);
  const list = await call(first, "/providers");
  const providers = (await list.json()) as {
    id: string;
    apiKeySecretRef?: string;
  }[];
  expect(providers).toHaveLength(1);
  expect(providers[0]?.apiKeySecretRef).toBeString();
  expect(values.size).toBe(1);
  const chatId = await turn(first);
  const state = (await first.service.getState(chatId)) as {
    descriptor: {
      profile: { id: string; digest: string };
      profileSnapshot: { tools: string[] };
    };
  };
  expect(state.descriptor.profile.id).toBe("desktop-host");
  expect(state.descriptor.profile.digest).toMatch(/^[a-f0-9]{64}$/);
  expect(state.descriptor.profileSnapshot.tools).toEqual([
    "example_echo",
    "example_now",
  ]);
  expect(seen).toEqual(["Bearer host-only-secret"]);
  expect(approvals[0]?.payloadJson).toContain("hello");
  await first.stop();
  const second = await buildApp({
    dbPath,
    env: { ...env, AGENTKIT_API_KEY: "must-not-reseed" },
    providerMode: "managed",
    providerPorts: { secrets, consent },
  });
  apps.push(second);
  await turn(second);
  expect(seen).toEqual(["Bearer host-only-secret", "Bearer host-only-secret"]);
  expect([...values.values()]).toEqual(["host-only-secret"]);
  const refresh = await call(
    second,
    "/providers/default/models/refresh",
    "POST",
    {},
  );
  expect(refresh.status).toBe(200);
  const models = await call(second, "/providers/default/models");
  expect(JSON.stringify(await models.json())).toContain("managed-model");
  const beforeDenial = seen.length;
  approved = false;
  const refused = await call(second, "/providers/default/test", "POST", {});
  expect(refused.ok).toBe(false);
  expect(seen.length).toBe(beforeDenial);
  await second.stop();
  expect(readFileSync(dbPath).includes(Buffer.from("host-only-secret"))).toBe(
    false,
  );
  expect(readFileSync(dbPath).includes(Buffer.from("must-not-reseed"))).toBe(
    false,
  );
}, 20_000);
