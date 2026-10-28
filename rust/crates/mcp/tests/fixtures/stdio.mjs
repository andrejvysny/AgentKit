import { appendFileSync } from "node:fs";

// The repo installs the Model Context Protocol SDK only under the pinned
// workspace alias `mcp-sdk-v1` (see the `mcp-sdk-v1:
// "npm:@modelcontextprotocol/sdk@1.30.0"` devDependency in the
// packages/mcp-client / packages/mcp-server manifests and bun.lock); the bare
// `@modelcontextprotocol/sdk` specifier is NOT installed and importing it
// makes this fixture die before speaking a single MCP frame. Same specifier
// pattern as packages/mcp-client/tests/fixtures/mcp-peer.mjs.
//
// The Rust side collapses any failed stdio connection into the opaque
// `McpClientError::Connect`, so the only diagnosable trace of a broken
// fixture is what this process writes to stderr: resolve every SDK import
// failure into an explicit, actionable message before exiting.
let Server;
let StdioServerTransport;
let CallToolRequestSchema;
let ListToolsRequestSchema;
try {
  ({ Server } = await import("mcp-sdk-v1/server/index.js"));
  ({ StdioServerTransport } = await import("mcp-sdk-v1/server/stdio.js"));
  ({ CallToolRequestSchema, ListToolsRequestSchema } = await import(
    "mcp-sdk-v1/types.js"
  ));
} catch (error) {
  console.error(
    "agentkit stdio MCP fixture (stdio.mjs): cannot load the MCP server SDK " +
      "from `mcp-sdk-v1` (the workspace alias for " +
      "npm:@modelcontextprotocol/sdk@1.30.0). This exact error otherwise " +
      "surfaces to the Rust test only as `McpClientError::Connect`. Run " +
      "`bun install` at the repository root so the workspace devDependency " +
      "is present, and make sure a bun binary is on PATH (this fixture is " +
      "launched as `bun <fixture>`). " +
      `Underlying error: ${error instanceof Error ? `${error.name}${error.code ? ` (${error.code})` : ""}: ${error.message}` : String(error)}`,
  );
  process.exit(1);
}

const server = new Server(
  { name: "agentkit-native-fixture", version: "1" },
  { capabilities: { tools: {} } },
);
server.setRequestHandler(ListToolsRequestSchema, async (request) =>
  process.env.COLLISION
    ? {
        tools: ["a.b", "a__b"].map((name) => ({
          name,
          inputSchema: { type: "object" },
        })),
      }
    : request.params?.cursor === "next"
      ? {
          tools: [{ name: "write", inputSchema: { type: "object" } }],
          ...(process.env.LOOP ? { nextCursor: "next" } : {}),
        }
      : {
          tools: [
            {
              name: "first",
              annotations: { readOnlyHint: true },
              inputSchema: {
                type: "object",
                properties: { large: { type: "boolean" } },
              },
            },
          ],
          nextCursor: "next",
        },
);
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  if (request.params.name === "first") {
    return {
      content: [
        {
          type: "text",
          text: request.params.arguments?.large
            ? "雪🙂".repeat(1000)
            : "Native echo",
        },
        { type: "image", mimeType: "image/png", data: "AA==" },
      ],
      structuredContent: { kind: "echo" },
    };
  }
  appendFileSync(process.env.MARKER, "once\n");
  process.exit(0);
});
await server.connect(new StdioServerTransport());
