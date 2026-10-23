import { appendFileSync } from "node:fs";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

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
