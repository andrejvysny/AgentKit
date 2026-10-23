const era = process.argv[2];
if (era === "legacy") {
  const { Server } = await import("mcp-sdk-v1/server/index.js");
  const { StdioServerTransport } = await import("mcp-sdk-v1/server/stdio.js");
  const { ListToolsRequestSchema, CallToolRequestSchema } = await import(
    "mcp-sdk-v1/types.js"
  );
  const server = new Server(
    { name: "old-peer", version: "1" },
    { capabilities: { tools: {} } },
  );
  server.setRequestHandler(ListToolsRequestSchema, () => ({
    tools: [definition()],
  }));
  server.setRequestHandler(CallToolRequestSchema, (request) => ({
    content: [{ type: "text", text: JSON.stringify(request.params.arguments) }],
  }));
  await server.connect(new StdioServerTransport());
} else {
  const { Server, PROTOCOL_VERSION_META_KEY } = await import(
    "@modelcontextprotocol/server"
  );
  const { serveStdio } = await import("@modelcontextprotocol/server/stdio");
  await serveStdio(() => {
    const server = new Server(
      { name: "modern-peer", version: "1" },
      { capabilities: { tools: {} } },
    );
    server.setRequestHandler("tools/list", () => ({ tools: [definition()] }));
    server.setRequestHandler("tools/call", (request, context) => ({
      content: [
        {
          type: "text",
          text: JSON.stringify({
            arguments: request.params.arguments,
            protocol: context.mcpReq.envelope?.[PROTOCOL_VERSION_META_KEY],
          }),
        },
      ],
    }));
    return server;
  });
}
function definition() {
  return {
    name: "echo",
    inputSchema: { type: "object" },
    annotations: { readOnlyHint: true },
  };
}
