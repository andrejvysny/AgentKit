import { expect, test } from "bun:test";
import {
  githubConfig,
  githubConnector,
  planeConfig,
  planeConnector,
  normalizeConnectionSecrets,
} from "../src/index.js";
import { createSecretStore } from "./helpers.js";
import type { ConnectionRecord } from "../src/index.js";

function record(
  config = planeConfig("plane", "workspace", "pat"),
): ConnectionRecord {
  return {
    id: "plane",
    connectorId: "plane",
    accountId: "account",
    workspaceId: "workspace",
    revision: 1,
    status: "disconnected",
    config,
  };
}

test("reference configs use official endpoints and secret references", () => {
  const github = githubConfig("github", "github/pat");
  expect(github.transport).toEqual({
    kind: "http",
    url: "https://api.githubcopilot.com/mcp/",
    headers: { Authorization: "Bearer ${token}" },
  });
  githubConnector.validate?.({
    ...record(github),
    connectorId: "github",
    workspaceId: undefined,
  });
  expect(planeConfig("plane", "workspace").transport).toMatchObject({
    url: "https://mcp.plane.so/http/mcp",
  });
  expect(planeConfig("plane", "workspace", "pat").transport).toMatchObject({
    url: "https://mcp.plane.so/http/api-key/mcp",
    headers: {
      "x-workspace-slug": "workspace",
      Authorization: "Bearer ${token}",
    },
  });
});

test("Plane refuses HTTP workspace mismatches including OAuth and duplicate header casing", () => {
  for (const url of [
    "https://mcp.plane.so/http/mcp",
    "https://mcp.plane.so/http/api-key/mcp",
  ]) {
    expect(() =>
      planeConnector.validate?.(
        record({
          alias: "plane",
          transport: {
            kind: "http",
            url,
            headers: { "x-workspace-slug": "other" },
          },
        }),
      ),
    ).toThrow("matching the selected workspace");
    expect(() =>
      planeConnector.validate?.(
        record({
          alias: "plane",
          transport: {
            kind: "http",
            url,
            headers: {
              "x-workspace-slug": "workspace",
              "X-Workspace-Slug": "other",
            },
          },
        }),
      ),
    ).toThrow("matching the selected workspace");
  }
});

test("Plane stdio workspace remains explicit across secret normalization and reconciliation", async () => {
  const config = {
    alias: "plane",
    transport: {
      kind: "stdio" as const,
      command: "uvx",
      args: ["plane-mcp-server", "stdio"],
      env: { PLANE_WORKSPACE_SLUG: "workspace", PLANE_API_KEY: "private-pat" },
    },
  };
  planeConnector.validate?.(record(config));
  const normalized = await normalizeConnectionSecrets(
    "plane",
    config,
    createSecretStore(),
    undefined,
    "workspace",
  );
  planeConnector.validate?.(record(normalized));
  expect(JSON.stringify(normalized)).not.toContain("private-pat");
  expect(() =>
    planeConnector.validate?.(
      record({
        ...config,
        transport: {
          ...config.transport,
          env: { PLANE_WORKSPACE_SLUG: "other" },
        },
      }),
    ),
  ).toThrow("matching the selected workspace");
});

test("identity reads are locally curated; unknown actions remain approval-gated", () => {
  expect(githubConnector.classify("get_me", {})).toBe("read");
  expect(planeConnector.classify("member", { action: "me" })).toBe("read");
  expect(planeConnector.classify("workitem", { action: "list" })).toBe("read");
  expect(planeConnector.classify("workitem", { action: "create" })).toBe(
    "write",
  );
  expect(planeConnector.classify("workitem", { action: "me" })).toBe("write");
  expect(planeConnector.classify("unknown", { action: "list" })).toBe("write");
});

test("generic stdio normalization does not treat a workspace-like environment name as public", async () => {
  const normalized = await normalizeConnectionSecrets(
    "custom",
    {
      alias: "custom",
      transport: {
        kind: "stdio",
        command: "custom",
        env: { PLANE_WORKSPACE_SLUG: "private-value" },
      },
    },
    createSecretStore(),
  );
  expect(JSON.stringify(normalized)).not.toContain("private-value");
});
