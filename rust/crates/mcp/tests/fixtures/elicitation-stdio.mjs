import { Server } from "mcp-sdk-v1/server/index.js";
import { StdioServerTransport } from "mcp-sdk-v1/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "mcp-sdk-v1/types.js";

const server = new Server(
  { name: "elicitation-fixture", version: "1" },
  { capabilities: { tools: {} } },
);
server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [{ name: "ask", inputSchema: { type: "object" } }],
}));
server.setRequestHandler(CallToolRequestSchema, async (request, extra) => {
  if (request.params.arguments?.capabilities) {
    return {
      content: [
        { type: "text", text: JSON.stringify(server.getClientCapabilities()) },
      ],
    };
  }
  const response = await server.elicitInput(
    {
      mode: "form",
      message: "Choose value",
      requestedSchema: {
        type: "object",
        properties: { value: { type: "string" } },
        required: ["value"],
      },
    },
    { signal: extra.signal },
  );
  return { content: [{ type: "text", text: JSON.stringify(response) }] };
});
await server.connect(new StdioServerTransport());
