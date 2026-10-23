import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

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
      inputSchema: { type: "object", "x-fixture": payload },
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
