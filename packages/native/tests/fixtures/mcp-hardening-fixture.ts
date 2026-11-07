import { Server } from "mcp-sdk-v1/server/index.js";
import { StdioServerTransport } from "mcp-sdk-v1/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "mcp-sdk-v1/types.js";
import { writeFileSync } from "node:fs";

if (process.env.READY_FILE)
  writeFileSync(process.env.READY_FILE, String(process.pid));
if (process.env.INIT_DELAY_MS)
  await Bun.sleep(Number(process.env.INIT_DELAY_MS));
const server = new Server(
  { name: "hardening", version: "1" },
  { capabilities: { tools: {} } },
);
server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    { name: "echo", inputSchema: { type: "object" } },
    { name: "ask", inputSchema: { type: "object" } },
  ],
}));
server.setRequestHandler(CallToolRequestSchema, async (request, extra) => {
  const answer =
    request.params.name === "ask"
      ? await server.elicitInput(
          {
            mode: "form",
            message: "Choose",
            requestedSchema: {
              type: "object",
              properties: { value: { type: "string" } },
              required: ["value"],
            },
          },
          { signal: extra.signal },
        )
      : { echo: true };
  return { content: [{ type: "text", text: JSON.stringify(answer) }] };
});
await server.connect(new StdioServerTransport());
