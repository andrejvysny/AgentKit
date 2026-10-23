import type { ConnectionRecord, ConnectorDefinition } from "./connections.js";
import type { McpServerConfig } from "./config.js";
import { McpError } from "./errors.js";

const invalid = (message: string): never => {
  throw new McpError("mcp_invalid_config", message);
};
function requireHttp(record: ConnectionRecord, allowed: readonly string[]) {
  if (
    record.config.transport.kind !== "http" ||
    !allowed.includes(record.config.transport.url)
  )
    invalid("Unexpected connector endpoint.");
}
/** Reads are curated locally. Unknown/new actions always require write approval. */
const githubReads = new Set([
  "get_me",
  "get_file_contents",
  "search_code",
  "search_repositories",
  "list_issues",
  "list_pull_requests",
  "search_issues",
  "search_pull_requests",
  "list_commits",
  "get_commit",
  "list_branches",
  "list_tags",
]);
const githubReadActions: Record<string, readonly string[]> = {
  issue_read: ["get", "get_comments", "get_sub_issues", "get_labels"],
  pull_request_read: [
    "get",
    "get_diff",
    "get_status",
    "get_files",
    "get_comments",
    "get_reviews",
    "get_review_comments",
  ],
};
export const githubConnector: ConnectorDefinition = {
  id: "github",
  label: "GitHub",
  transports: ["http"],
  capabilityDescriptions: [
    "Repository context and explicitly approved issue/PR changes",
  ],
  authMethods: ["token", "oauth"],
  classify(name, args) {
    if (githubReads.has(name)) return "read";
    return typeof args.method === "string" &&
      githubReadActions[name]?.includes(args.method)
      ? "read"
      : "write";
  },
  validate(record) {
    requireHttp(record, ["https://api.githubcopilot.com/mcp/"]);
  },
};
const planeResources = new Set([
  "workitem",
  "project",
  "page",
  "cycle",
  "module",
  "workspace",
  "user",
  "label",
  "state",
  "comment",
]);
const planeReads = new Set(["list", "get", "retrieve", "search"]);
export const planeConnector: ConnectorDefinition = {
  id: "plane",
  label: "Plane",
  transports: ["http", "stdio"],
  capabilityDescriptions: [
    "Workspace-bound project, work-item and Page actions",
  ],
  authMethods: ["token", "oauth"],
  classify(name, args) {
    return planeResources.has(name) &&
      typeof args.action === "string" &&
      planeReads.has(args.action)
      ? "read"
      : "write";
  },
  validate(record) {
    if (!record.workspaceId)
      invalid("Plane connections require an explicit workspace.");
    if (record.config.transport.kind === "stdio") return;
    requireHttp(record, [
      "https://mcp.plane.so/http/mcp",
      "https://mcp.plane.so/http/api-key/mcp",
    ]);
    if (
      record.config.transport.url.endsWith("/api-key/mcp") &&
      record.config.transport.headers?.["x-workspace-slug"] !==
        record.workspaceId
    )
      invalid(
        "Plane PAT connections require x-workspace-slug matching the selected workspace.",
      );
  },
};
export const customMcpConnector: ConnectorDefinition = {
  id: "custom-mcp",
  label: "Custom MCP",
  transports: ["http", "stdio"],
  authMethods: ["token", "oauth", "none"],
  classify: () => "write",
};
export const referenceConnectors = [
  githubConnector,
  planeConnector,
  customMcpConnector,
] as const;
export function githubConfig(
  alias: string,
  tokenRef?: string,
): McpServerConfig {
  return {
    alias,
    transport: {
      kind: "http",
      url: "https://api.githubcopilot.com/mcp/",
      ...(tokenRef ? { headers: { Authorization: `Bearer \${token}` } } : {}),
    },
    ...(tokenRef ? { secretRefs: { token: tokenRef } } : {}),
  };
}
export function planeConfig(
  alias: string,
  workspace: string,
  tokenRef?: string,
): McpServerConfig {
  if (!workspace.trim()) invalid("Plane workspace is required.");
  return {
    alias,
    transport: {
      kind: "http",
      url: tokenRef
        ? "https://mcp.plane.so/http/api-key/mcp"
        : "https://mcp.plane.so/http/mcp",
      headers: {
        "x-workspace-slug": workspace,
        ...(tokenRef ? { Authorization: `Bearer \${token}` } : {}),
      },
    },
    ...(tokenRef ? { secretRefs: { token: tokenRef } } : {}),
  };
}
