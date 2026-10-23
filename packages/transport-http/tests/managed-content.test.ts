import { expect, test } from "bun:test";
import {
  ConnectionService,
  customMcpConnector,
} from "../../mcp-client/src/index.js";
import {
  MemoryConnectionStore,
  MemorySecretStore,
  MemoryPromptTemplateStore,
} from "@agentkit/adapters-memory";
import { PromptTemplateService } from "@agentkit/host";
import { createHandlerFixture, request } from "./support/fixture.js";

test("managed routes expose lifecycle, normalize secrets and render immutable templates", async () => {
  const connections = new ConnectionService({
    store: new MemoryConnectionStore(),
    secrets: new MemorySecretStore(),
    connectors: [customMcpConnector],
    recordOperation: async () => {},
  });
  const templates = new PromptTemplateService(new MemoryPromptTemplateStore());
  const f = await createHandlerFixture({
    connections,
    promptTemplates: templates,
  });
  try {
    const saved = await f.handler(
      request("PATCH", "/v1/connections/one", {
        body: {
          connectorId: "custom-mcp",
          accountId: "account",
          revision: 1,
          expectedRevision: null,
          config: {
            alias: "test",
            enabled: false,
            transport: {
              kind: "http",
              url: "https://service.test/mcp",
              headers: { Authorization: "literal-token" },
            },
          },
        },
      }),
    );
    expect(saved.status).toBe(200);
    expect(await saved.text()).not.toContain("literal-token");
    const listed = await f.handler(request("GET", "/v1/connections"));
    expect(listed.status).toBe(200);
    expect(await listed.text()).not.toContain("literal-token");
    const published = await f.handler(
      request("POST", "/v1/prompt-templates", {
        body: {
          id: "review",
          revision: "1",
          description: "test",
          argumentsSchema: {
            type: "object",
            properties: { name: { type: "string" } },
            required: ["name"],
          },
          messages: [{ role: "user", content: "Review {{name}}" }],
        },
      }),
    );
    expect(published.status).toBe(201);
    const render = await f.handler(
      request("POST", "/v1/prompt-templates/review/1/render", {
        body: { name: "AgentKit" },
      }),
    );
    expect(render.status).toBe(200);
    expect(await render.json()).toMatchObject({
      messages: [{ role: "user", content: "Review AgentKit" }],
    });
    expect(
      (
        await f.handler(
          request("POST", "/v1/prompt-templates/review/1/render", { body: {} }),
        )
      ).status,
    ).toBe(400);
    expect(
      (await f.handler(request("DELETE", "/v1/connections/one"))).status,
    ).toBe(204);
  } finally {
    await connections.dispose();
  }
});
test("optional managed services explicitly return 501", async () => {
  const { handler } = await createHandlerFixture();
  for (const path of ["/v1/connections", "/v1/prompt-templates", "/v1/skills"])
    expect((await handler(request("GET", path))).status).toBe(501);
});
