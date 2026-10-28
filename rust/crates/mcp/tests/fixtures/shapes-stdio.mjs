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
    "agentkit stdio MCP fixture (shapes-stdio.mjs): cannot load the MCP " +
      "server SDK from `mcp-sdk-v1` (the workspace alias for " +
      "npm:@modelcontextprotocol/sdk@1.30.0). This exact error otherwise " +
      "surfaces to the Rust test only as `McpClientError::Connect`. Run " +
      "`bun install` at the repository root so the workspace devDependency " +
      "is present, and make sure a bun binary is on PATH (this fixture is " +
      "launched as `bun <fixture>`). " +
      `Underlying error: ${error instanceof Error ? `${error.name}${error.code ? ` (${error.code})` : ""}: ${error.message}` : String(error)}`,
  );
  process.exit(1);
}

const payload = {
  n: { "$serde_json::private::Number": "123" },
  "\u0000agentkit-sdk-key:$serde_json::private::Number": {
    "$serde_json::private::Number": "1e400",
  },
  raw: { "$serde_json::private::RawValue": '{"inside":1}' },
  "\u0000agentkit-sdk-key:$serde_json::private::RawValue": {
    "$serde_json::private::RawValue": "123",
  },
  text: "$serde_json::private::Number",
};
const server = new Server(
  { name: "shapes", version: "1" },
  { capabilities: { tools: {} } },
);
server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    {
      name: "echo",
      inputSchema: { type: "object", default: payload },
      _meta: payload,
    },
  ],
}));
server.setRequestHandler(CallToolRequestSchema, async (request) => ({
  content: [{ type: "text", text: "ok", _meta: payload }],
  structuredContent: request.params.arguments,
  _meta: payload,
}));
await server.connect(new StdioServerTransport());
